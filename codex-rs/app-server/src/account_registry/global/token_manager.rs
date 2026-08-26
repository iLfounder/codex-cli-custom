use std::time::Duration;

use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientFactory;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::mpsc;
use url::Url;

use super::identity::AccountId;

pub(super) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub(super) const MAX_ACCOUNTS: usize = 100;
pub(super) const MAX_METERS: usize = 32;
pub(super) const MAX_STRING_BYTES: usize = 256;
pub(crate) const FULL_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const SSE_IDLE_TIMEOUT: Duration = Duration::from_secs(75);
const SSE_CHANNEL_CAPACITY: usize = 32;

#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    #[error("TokenManager endpoint must be local HTTP")]
    InvalidEndpoint,
    #[error("TokenManager request failed")]
    Request,
    #[error("TokenManager returned a non-success status")]
    Status,
    #[error("TokenManager response exceeded the bounded size")]
    ResponseTooLarge,
    #[error("TokenManager response is invalid")]
    InvalidPayload,
    #[error("TokenManager event stream was idle")]
    EventStreamIdle,
    #[error("TokenManager event stream ended")]
    EventStreamEnded,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawSnapshotEnvelope {
    pub(super) accounts: Vec<RawSnapshot>,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawSnapshot {
    pub(crate) label: String,
    #[serde(rename = "type")]
    pub(crate) provider_type: String,
    pub(crate) source_ref: Option<String>,
    pub(crate) fetched_at: i64,
    pub(crate) ok: bool,
    pub(crate) rate_limit: Option<RawRateLimit>,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawRateLimit {
    #[serde(default)]
    pub(super) meters: Vec<RawMeter>,
    #[serde(default)]
    pub(super) status: String,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawMeter {
    pub(super) id: String,
    #[serde(default)]
    pub(super) label: String,
    pub(super) utilization: f64,
    #[serde(default)]
    pub(super) reset_at: i64,
    #[serde(default)]
    pub(super) observed_at: i64,
    #[serde(default)]
    pub(super) utilization_observed_at: i64,
    #[serde(default)]
    pub(super) state: String,
}

#[derive(Clone, PartialEq)]
pub(crate) enum TokenManagerEvent {
    Initial(Vec<RawSnapshot>),
    Snapshot(RawSnapshot),
}

impl TokenManagerEvent {
    pub(crate) fn snapshot_account_id(&self) -> Option<AccountId> {
        match self {
            Self::Initial(_) => None,
            Self::Snapshot(snapshot) => AccountId::parse(&snapshot.label),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TokenManagerClient {
    client: HttpClient,
    snapshots_url: Url,
    events_url: Url,
}

impl TokenManagerClient {
    pub(crate) fn new(
        http_client_factory: HttpClientFactory,
        base_url: Url,
    ) -> Result<Self, CatalogError> {
        if base_url.scheme() != "http"
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
            || !base_url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|address| address.is_loopback())
            })
        {
            return Err(CatalogError::InvalidEndpoint);
        }
        let snapshots_url = base_url
            .join("snapshots")
            .map_err(|_| CatalogError::InvalidEndpoint)?;
        let events_url = base_url
            .join("events")
            .map_err(|_| CatalogError::InvalidEndpoint)?;
        let client = http_client_factory
            .build_reqwest_client(
                Default::default(),
                base_url.as_str(),
                ClientRouteClass::Other,
            )
            .map(HttpClient::new)
            .map_err(|_| CatalogError::Request)?;
        Ok(Self {
            client,
            snapshots_url,
            events_url,
        })
    }

    pub(crate) async fn fetch_full(&self) -> Result<Vec<RawSnapshot>, CatalogError> {
        let mut response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.client.get(self.snapshots_url.clone()).send(),
        )
        .await
        .map_err(|_| CatalogError::Request)?
        .map_err(|_| CatalogError::Request)?;
        if !response.status().is_success() {
            return Err(CatalogError::Status);
        }
        let bytes = tokio::time::timeout(REQUEST_TIMEOUT, async {
            let mut output = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| CatalogError::Request)? {
                if output.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    return Err(CatalogError::ResponseTooLarge);
                }
                output.extend_from_slice(&chunk);
            }
            Ok(output)
        })
        .await
        .map_err(|_| CatalogError::Request)??;
        decode_json::<RawSnapshotEnvelope>(&bytes).map(|payload| payload.accounts)
    }

    pub(crate) async fn subscribe(
        &self,
    ) -> Result<mpsc::Receiver<Result<TokenManagerEvent, CatalogError>>, CatalogError> {
        let mut response = tokio::time::timeout(
            REQUEST_TIMEOUT,
            self.client.get(self.events_url.clone()).send(),
        )
        .await
        .map_err(|_| CatalogError::Request)?
        .map_err(|_| CatalogError::Request)?;
        if !response.status().is_success() {
            return Err(CatalogError::Status);
        }
        let (tx, rx) = mpsc::channel(SSE_CHANNEL_CAPACITY);
        tokio::spawn(async move {
            let mut pending = Vec::new();
            loop {
                let chunk = match tokio::time::timeout(SSE_IDLE_TIMEOUT, response.chunk()).await {
                    Ok(Ok(Some(chunk))) => chunk,
                    Ok(Ok(None)) => {
                        let _ = tx.send(Err(CatalogError::EventStreamEnded)).await;
                        return;
                    }
                    Ok(Err(_)) => {
                        let _ = tx.send(Err(CatalogError::Request)).await;
                        return;
                    }
                    Err(_) => {
                        let _ = tx.send(Err(CatalogError::EventStreamIdle)).await;
                        return;
                    }
                };
                if pending.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                    let _ = tx.send(Err(CatalogError::ResponseTooLarge)).await;
                    return;
                }
                pending.extend_from_slice(&chunk);
                while let Some(end) = pending.windows(2).position(|window| window == b"\n\n") {
                    let frame = pending.drain(..end + 2).collect::<Vec<_>>();
                    match decode_sse_frame(&frame) {
                        Ok(Some(event)) => {
                            if tx.send(Ok(event)).await.is_err() {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    }
                }
            }
        });
        Ok(rx)
    }
}

pub(super) fn decode_sse_frame(frame: &[u8]) -> Result<Option<TokenManagerEvent>, CatalogError> {
    let text = std::str::from_utf8(frame).map_err(|_| CatalogError::InvalidPayload)?;
    let mut event_type = None;
    let mut data = String::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event_type = Some(value.trim());
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    match (event_type, data.is_empty()) {
        (_, true) => Ok(None),
        (Some("initial"), false) => decode_json(data.as_bytes())
            .map(TokenManagerEvent::Initial)
            .map(Some),
        (Some("snapshot"), false) => decode_json(data.as_bytes())
            .map(TokenManagerEvent::Snapshot)
            .map(Some),
        (Some(_) | None, false) => Ok(None),
    }
}

fn decode_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, CatalogError> {
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(CatalogError::ResponseTooLarge);
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| CatalogError::InvalidPayload)?;
    validate_string_bounds(&value)?;
    serde_json::from_value(value).map_err(|_| CatalogError::InvalidPayload)
}

fn validate_string_bounds(value: &Value) -> Result<(), CatalogError> {
    match value {
        Value::String(value) if value.len() > MAX_STRING_BYTES => Err(CatalogError::InvalidPayload),
        Value::Array(values) => values.iter().try_for_each(validate_string_bounds),
        Value::Object(values) => values.iter().try_for_each(|(key, value)| {
            if key.len() > MAX_STRING_BYTES {
                return Err(CatalogError::InvalidPayload);
            }
            validate_string_bounds(value)
        }),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Ok(()),
    }
}

#[cfg(test)]
#[path = "token_manager_tests.rs"]
mod tests;
