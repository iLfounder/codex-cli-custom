use std::fmt;
use std::io;
#[cfg(unix)]
use std::io::BufRead;
#[cfg(unix)]
use std::io::BufReader;
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClient;
use codex_http_client::HttpClientFactory;
use codex_login::ExternalAuthFuture;
use codex_login::auth::ReadOnlyAuthRefresh;
use serde::Deserialize;
#[cfg(unix)]
use serde::Serialize;
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
const MAX_CONTROL_RESPONSE_BYTES: usize = 4096;
const CONTROL_SOCKET_RELATIVE_PATH: &str = ".tokenmanager/control/tokenmanager.sock";

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
    pub(crate) meters: Vec<RawMeter>,
    #[serde(default)]
    pub(crate) status: String,
}

#[derive(Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawMeter {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) label: String,
    pub(crate) utilization: f64,
    #[serde(default)]
    pub(crate) reset_at: i64,
    #[serde(default)]
    pub(crate) observed_at: i64,
    #[serde(default)]
    pub(crate) utilization_observed_at: i64,
    #[serde(default)]
    pub(crate) state: String,
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

#[derive(Clone)]
pub(crate) struct TokenManagerClient {
    client: HttpClient,
    snapshots_url: Url,
    events_url: Url,
    control: Option<Arc<TokenManagerControl>>,
}

impl fmt::Debug for TokenManagerClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenManagerClient")
            .field("snapshots_url", &self.snapshots_url)
            .field("events_url", &self.events_url)
            .field("control_available", &self.control.is_some())
            .finish_non_exhaustive()
    }
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
            control: super::directory::GlobalAccountDirectory::user_home().map(|home| {
                Arc::new(TokenManagerControl::new(
                    home.join(CONTROL_SOCKET_RELATIVE_PATH),
                ))
            }),
        })
    }

    pub(crate) fn read_only_auth_refresh(
        &self,
        account_id: AccountId,
    ) -> Result<Arc<dyn ReadOnlyAuthRefresh>, CatalogError> {
        let control = self.control.clone().ok_or(CatalogError::Request)?;
        Ok(Arc::new(TokenManagerAccountRefresh {
            control,
            account_id,
        }))
    }

    // Consumed by the serial global lifecycle authority after this primitive lands.
    #[allow(dead_code)]
    pub(crate) async fn begin_lifecycle(
        &self,
        account_id: AccountId,
    ) -> Result<TokenManagerLifecycle, CatalogError> {
        let control = self.control.clone().ok_or(CatalogError::Request)?;
        tokio::task::spawn_blocking(move || control.begin(account_id))
            .await
            .map_err(|_| CatalogError::Request)?
            .map_err(|_| CatalogError::Request)
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

struct TokenManagerAccountRefresh {
    control: Arc<TokenManagerControl>,
    account_id: AccountId,
}

impl ReadOnlyAuthRefresh for TokenManagerAccountRefresh {
    fn force_refresh(&self) -> ExternalAuthFuture<'_, ()> {
        let control = Arc::clone(&self.control);
        let account_id = self.account_id;
        Box::pin(async move {
            tokio::task::spawn_blocking(move || control.force_refresh(account_id))
                .await
                .map_err(|_| io::Error::other("TokenManager control worker stopped"))?
                .map_err(|error| {
                    io::Error::new(error.kind(), "TokenManager control request failed")
                })
        })
    }
}

struct TokenManagerControl {
    socket_path: PathBuf,
}

impl TokenManagerControl {
    fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    #[cfg(unix)]
    fn begin(&self, account_id: AccountId) -> io::Result<TokenManagerLifecycle> {
        use std::os::unix::net::UnixStream;

        let stream = UnixStream::connect(&self.socket_path)?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let mut connection = BufReader::new(stream);
        let account_id = account_id.to_string();
        let begin = control_exchange(&mut connection, "lifecycle/begin", &account_id)?;
        let generation = begin
            .ok
            .then_some(begin.generation)
            .flatten()
            .filter(|_| matches!(begin.state.as_deref(), Some("active" | "absent")))
            .ok_or_else(invalid_control_response)?;
        Ok(TokenManagerLifecycle {
            connection: Some(connection),
            account_id,
            generation,
        })
    }

    #[cfg(not(unix))]
    fn begin(&self, _account_id: AccountId) -> io::Result<TokenManagerLifecycle> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "TokenManager control is unavailable on this platform",
        ))
    }

    #[cfg(unix)]
    fn force_refresh(&self, account_id: AccountId) -> io::Result<()> {
        let mut lifecycle = self.begin(account_id)?;
        let account_id = lifecycle.account_id.clone();
        let generation = lifecycle.generation;
        let connection = lifecycle.connection_mut()?;
        let force = control_exchange(connection, "lifecycle/forceRefresh", &account_id);
        let force_valid = force.as_ref().is_ok_and(|response| {
            response.ok
                && response.state.as_deref() == Some("refreshed")
                && response.generation == Some(generation)
        });
        if !force_valid {
            lifecycle.abort_sync();
            return Err(force.err().unwrap_or_else(invalid_control_response));
        }
        lifecycle.commit_sync()
    }

    #[cfg(not(unix))]
    fn force_refresh(&self, _account_id: AccountId) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "TokenManager control is unavailable on this platform",
        ))
    }
}

pub(crate) struct TokenManagerLifecycle {
    #[cfg(unix)]
    connection: Option<BufReader<std::os::unix::net::UnixStream>>,
    #[cfg(not(unix))]
    connection: Option<()>,
    account_id: String,
    generation: u64,
}

impl fmt::Debug for TokenManagerLifecycle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenManagerLifecycle")
            .field("account_id", &self.account_id)
            .field("generation", &self.generation)
            .field("active", &self.connection.is_some())
            .finish()
    }
}

impl TokenManagerLifecycle {
    #[cfg(unix)]
    fn connection_mut(&mut self) -> io::Result<&mut BufReader<std::os::unix::net::UnixStream>> {
        self.connection
            .as_mut()
            .ok_or_else(|| io::Error::other("TokenManager lifecycle is closed"))
    }

    #[cfg(unix)]
    fn finish_sync(&mut self, method: &'static str, expected_state: &str) -> io::Result<()> {
        let account_id = self.account_id.clone();
        let response = control_exchange(self.connection_mut()?, method, &account_id);
        let valid = response
            .as_ref()
            .is_ok_and(|response| response.ok && response.state.as_deref() == Some(expected_state));
        self.connection.take();
        if valid {
            Ok(())
        } else {
            Err(response.err().unwrap_or_else(invalid_control_response))
        }
    }

    #[cfg(unix)]
    fn commit_sync(&mut self) -> io::Result<()> {
        self.finish_sync("lifecycle/commit", "committed")
    }

    #[cfg(unix)]
    fn abort_sync(&mut self) {
        let _ = self.finish_sync("lifecycle/abort", "aborted");
    }

    // Consumed by the serial global lifecycle authority after this primitive lands.
    #[allow(dead_code)]
    pub(crate) async fn commit(self) -> Result<(), CatalogError> {
        tokio::task::spawn_blocking(move || {
            #[cfg(unix)]
            {
                let mut lifecycle = self;
                lifecycle.commit_sync()
            }
            #[cfg(not(unix))]
            {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "TokenManager control is unavailable on this platform",
                ))
            }
        })
        .await
        .map_err(|_| CatalogError::Request)?
        .map_err(|_| CatalogError::Request)
    }

    // Consumed by the serial global lifecycle authority after this primitive lands.
    #[allow(dead_code)]
    pub(crate) async fn abort(mut self) {
        let _ = tokio::task::spawn_blocking(move || {
            #[cfg(unix)]
            self.abort_sync();
            #[cfg(not(unix))]
            self.connection.take();
        })
        .await;
    }
}

#[cfg(unix)]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ControlRequest<'a> {
    method: &'static str,
    account_id: &'a str,
}

#[cfg(unix)]
#[derive(Deserialize)]
struct ControlResponse {
    ok: bool,
    state: Option<String>,
    generation: Option<u64>,
}

#[cfg(unix)]
fn control_exchange(
    connection: &mut BufReader<std::os::unix::net::UnixStream>,
    method: &'static str,
    account_id: &str,
) -> io::Result<ControlResponse> {
    let writer = connection.get_mut();
    serde_json::to_writer(&mut *writer, &ControlRequest { method, account_id })
        .map_err(|_| invalid_control_response())?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let mut line = Vec::new();
    let read = connection
        .by_ref()
        .take((MAX_CONTROL_RESPONSE_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)?;
    if read == 0 || line.len() > MAX_CONTROL_RESPONSE_BYTES || !line.ends_with(b"\n") {
        return Err(invalid_control_response());
    }
    serde_json::from_slice(&line).map_err(|_| invalid_control_response())
}

fn invalid_control_response() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "TokenManager control response is invalid",
    )
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
