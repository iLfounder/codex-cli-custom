use std::time::Duration;

use anyhow::Context;
use codex_app_server_client::AppServerEvent;
use codex_app_server_client::RemoteAppServerClient;
use codex_app_server_client::RemoteAppServerConnectArgs;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol::AccountSlotCatalogKind;
use codex_app_server_protocol::AccountSlotListParams;
use codex_app_server_protocol::AccountSlotListResponse;
use codex_app_server_protocol::AccountSlotLoginChallenge;
use codex_app_server_protocol::AccountSlotLoginStartParams;
use codex_app_server_protocol::AccountSlotLoginStartResponse;
use codex_app_server_protocol::AccountSlotLogoutParams;
use codex_app_server_protocol::AccountSlotLogoutResponse;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::CancelLoginAccountParams;
use codex_app_server_protocol::CancelLoginAccountResponse;
use codex_app_server_protocol::CancelLoginAccountStatus;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_utils_home_dir::ManagedAccountCatalog;
use codex_utils_home_dir::ManagedAccountId;
use reqwest::StatusCode;
use reqwest::header::LOCATION;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::time::Instant;
use url::Url;

use crate::FeatureToggles;
use crate::InteractiveRemoteOptions;
use crate::LoginCommand;
use crate::LoginSubcommand;
use crate::Subcommand;
use codex_utils_cli::CliConfigOverrides;

const MANAGED_ACCOUNT_ENV: &str = "CODEX_MANAGED_ACCOUNT_ID";
const MAX_ACCOUNT_PAGES: usize = 4;
const ACCOUNT_PAGE_LIMIT: u32 = 100;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(2 * 60);

#[derive(Clone, Copy)]
enum LifecycleAction<'a> {
    Login(&'a LoginCommand),
    Status,
    Logout,
}

pub(super) async fn run_if_managed(
    root_config: &CliConfigOverrides,
    features: &FeatureToggles,
    remote: &InteractiveRemoteOptions,
    interactive: &codex_tui::Cli,
    subcommand: &Option<Subcommand>,
) -> anyhow::Result<bool> {
    let Some(action) = lifecycle_action(subcommand) else {
        return Ok(false);
    };
    let Some(hint) = std::env::var_os(MANAGED_ACCOUNT_ENV) else {
        return Ok(false);
    };
    validate_invocation(root_config, features, remote, interactive, action)?;
    let hint = hint
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("managed account hint is malformed"))?;
    let codex_home = codex_utils_home_dir::find_codex_home()
        .map_err(|_| anyhow::anyhow!("managed CODEX_HOME cannot be resolved"))?;
    let account_id = ManagedAccountCatalog::load()
        .context("managed account catalog could not be loaded")?
        .match_hint(hint, codex_home.as_path())?;
    let socket_path = codex_app_server_client::canonical_app_server_control_socket_path()
        .map_err(|_| anyhow::anyhow!("canonical app-server endpoint is unavailable"))?;
    let client = RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
        endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
        client_name: "codex_cli_managed_account".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        experimental_api: false,
        mcp_server_openai_form_elicitation: false,
        opt_out_notification_methods: Vec::new(),
        channel_capacity: 32,
    })
    .await
    .map_err(|_| anyhow::anyhow!("canonical app-server is unavailable"))?;

    let result = run_action(client, account_id, action).await;
    result.map(|()| true)
}

fn lifecycle_action(subcommand: &Option<Subcommand>) -> Option<LifecycleAction<'_>> {
    match subcommand {
        Some(Subcommand::Login(command)) => match command.action {
            Some(LoginSubcommand::Status) => Some(LifecycleAction::Status),
            None => Some(LifecycleAction::Login(command)),
        },
        Some(Subcommand::Logout(_)) => Some(LifecycleAction::Logout),
        _ => None,
    }
}

fn validate_invocation(
    root_config: &CliConfigOverrides,
    features: &FeatureToggles,
    remote: &InteractiveRemoteOptions,
    interactive: &codex_tui::Cli,
    action: LifecycleAction<'_>,
) -> anyhow::Result<()> {
    if !root_config.raw_overrides.is_empty()
        || !features.enable.is_empty()
        || !features.disable.is_empty()
        || interactive.strict_config
        || interactive.config_profile_v2.is_some()
    {
        anyhow::bail!("managed account lifecycle does not accept configuration overrides");
    }
    if remote.remote.is_some() || remote.remote_auth_token_env.is_some() {
        anyhow::bail!("managed account lifecycle always uses the canonical local app-server");
    }
    if let LifecycleAction::Login(command) = action
        && (command.with_api_key
            || command.with_access_token
            || command.api_key.is_some()
            || command.use_device_code
            || command.issuer_base_url.is_some()
            || command.client_id.is_some())
    {
        anyhow::bail!("managed account login only supports browser authentication");
    }
    Ok(())
}

async fn run_action(
    client: RemoteAppServerClient,
    account_id: ManagedAccountId,
    action: LifecycleAction<'_>,
) -> anyhow::Result<()> {
    let slot = exact_account_slot(&client, account_id).await?;
    match action {
        LifecycleAction::Status => {
            println!("{account_id}: {}", status_name(slot.status));
            shutdown(client).await
        }
        LifecycleAction::Logout => {
            let _: AccountSlotLogoutResponse = client
                .request_typed(ClientRequest::AccountSlotLogout {
                    request_id: RequestId::Integer(2),
                    params: AccountSlotLogoutParams {
                        account_slot_id: account_id.to_string(),
                        expected_registry_revision: slot.registry_revision,
                        expected_attempt_generation: slot.attempt_generation,
                    },
                })
                .await
                .map_err(|_| anyhow::anyhow!("managed account logout failed"))?;
            println!("{account_id} logged out.");
            shutdown(client).await
        }
        LifecycleAction::Login(_) => run_login(client, account_id).await,
    }
}

async fn exact_account_slot(
    client: &RemoteAppServerClient,
    account_id: ManagedAccountId,
) -> anyhow::Result<AccountSlotSnapshot> {
    let mut cursor = None;
    let mut revision = None;
    let mut matches = Vec::new();
    for page in 0..MAX_ACCOUNT_PAGES {
        let response: AccountSlotListResponse = client
            .request_typed(ClientRequest::AccountSlotList {
                request_id: RequestId::Integer(page as i64 + 1),
                params: AccountSlotListParams {
                    cursor,
                    limit: Some(ACCOUNT_PAGE_LIMIT),
                },
            })
            .await
            .map_err(|_| anyhow::anyhow!("managed account inventory is unavailable"))?;
        if response.catalog_kind != AccountSlotCatalogKind::Global
            || !response.multi_account.available
            || revision.is_some_and(|value| value != response.registry_revision)
        {
            anyhow::bail!("managed account inventory is unavailable");
        }
        revision = Some(response.registry_revision);
        matches.extend(response.data.into_iter().filter(|slot| {
            slot.account_slot_id == account_id.to_string()
                && slot.account_number == account_id.number()
                && slot.registry_revision == response.registry_revision
        }));
        let Some(next_cursor) = response.next_cursor else {
            return match matches.as_slice() {
                [slot] => Ok(slot.clone()),
                _ => anyhow::bail!("managed account inventory did not contain one exact account"),
            };
        };
        cursor = Some(next_cursor);
    }
    anyhow::bail!("managed account inventory exceeded its page limit")
}

async fn run_login(
    mut client: RemoteAppServerClient,
    account_id: ManagedAccountId,
) -> anyhow::Result<()> {
    let response: AccountSlotLoginStartResponse = client
        .request_typed(ClientRequest::AccountSlotLoginStart {
            request_id: RequestId::Integer(10),
            params: AccountSlotLoginStartParams::Chatgpt {
                slot_id: Some(account_id.to_string()),
                codex_streamlined_login: false,
                use_hosted_login_success_page: false,
                app_brand: None,
            },
        })
        .await
        .map_err(|_| anyhow::anyhow!("managed account login could not be started"))?;
    let Some(AccountSlotLoginChallenge::Browser { login_id, auth_url }) = response.challenge else {
        shutdown(client).await?;
        anyhow::bail!("managed account login returned an invalid browser challenge");
    };
    if response.slot.account_slot_id != account_id.to_string()
        || response.operation.operation_id != login_id
        || response.operation.account_slot_id.as_deref() != Some(account_id.to_string().as_str())
        || validate_auth_url(&auth_url).is_err()
    {
        cancel_login(&client, login_id).await;
        shutdown(client).await?;
        anyhow::bail!("managed account login returned an invalid browser challenge");
    }

    println!("Open this URL to authenticate {account_id}:\n{auth_url}");
    println!("Paste the final localhost callback URL, or wait for the automatic callback:");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut manual_input = true;
    let timeout = tokio::time::sleep_until(Instant::now() + LOGIN_TIMEOUT);
    tokio::pin!(timeout);
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    let result = loop {
        tokio::select! {
            event = client.next_event() => match event {
                Some(AppServerEvent::ServerNotification(notification)) => {
                    if let ServerNotification::SessionRuntimeOperationUpdated(notification) = *notification
                        && notification.operation.operation_id == login_id
                    {
                        match notification.operation.status {
                            SessionRuntimeOperationStatus::Ready => break Ok(()),
                            SessionRuntimeOperationStatus::Failed => {
                                break Err(anyhow::anyhow!("managed account login failed"));
                            }
                            SessionRuntimeOperationStatus::Accepted
                            | SessionRuntimeOperationStatus::Running
                            | SessionRuntimeOperationStatus::Released => {}
                        }
                    }
                }
                Some(AppServerEvent::Disconnected { .. }) | None => {
                    break Err(anyhow::anyhow!("canonical app-server disconnected during login"));
                }
                Some(
                    AppServerEvent::Connected { .. }
                    | AppServerEvent::Lagged { .. }
                    | AppServerEvent::ServerRequest(_),
                ) => {}
            },
            line = lines.next_line(), if manual_input => match line {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    if let Err(error) = deliver_callback(&auth_url, line.trim()).await {
                        break Err(error);
                    }
                    manual_input = false;
                    println!("Callback delivered; waiting for account confirmation.");
                }
                Ok(Some(_)) => {}
                Ok(None) => manual_input = false,
                Err(_) => break Err(anyhow::anyhow!("failed to read the callback URL")),
            },
            _ = &mut interrupt => break Err(anyhow::anyhow!("managed account login interrupted")),
            _ = &mut timeout => break Err(anyhow::anyhow!("managed account login timed out")),
        }
    };
    if result.is_err() {
        cancel_login(&client, login_id).await;
    } else {
        println!("{account_id} login completed.");
    }
    let close_result = shutdown(client).await;
    result.and(close_result)
}

async fn cancel_login(client: &RemoteAppServerClient, login_id: String) {
    let response = client
        .request_typed::<CancelLoginAccountResponse>(ClientRequest::CancelLoginAccount {
            request_id: RequestId::Integer(11),
            params: CancelLoginAccountParams { login_id },
        })
        .await;
    if let Ok(response) = response {
        match response.status {
            CancelLoginAccountStatus::Canceled | CancelLoginAccountStatus::NotFound => {}
        }
    }
}

async fn shutdown(client: RemoteAppServerClient) -> anyhow::Result<()> {
    client
        .shutdown()
        .await
        .map_err(|_| anyhow::anyhow!("canonical app-server connection did not close cleanly"))
}

fn status_name(status: codex_app_server_protocol::AccountSlotStatus) -> &'static str {
    match status {
        codex_app_server_protocol::AccountSlotStatus::LoginRequired => "login required",
        codex_app_server_protocol::AccountSlotStatus::Ready => "ready",
        codex_app_server_protocol::AccountSlotStatus::Failed => "failed",
    }
}

fn validate_auth_url(auth_url: &str) -> anyhow::Result<(Url, String)> {
    let auth =
        Url::parse(auth_url).map_err(|_| anyhow::anyhow!("authentication URL is invalid"))?;
    if auth.scheme() != "https"
        || auth.host_str().is_none()
        || !auth.username().is_empty()
        || auth.password().is_some()
        || auth.fragment().is_some()
    {
        anyhow::bail!("authentication URL is invalid");
    }
    let redirects = auth
        .query_pairs()
        .filter(|(key, _)| key == "redirect_uri")
        .collect::<Vec<_>>();
    let states = auth
        .query_pairs()
        .filter(|(key, _)| key == "state")
        .collect::<Vec<_>>();
    if redirects.len() != 1 || states.len() != 1 || states[0].1.is_empty() {
        anyhow::bail!("authentication URL omitted its callback identity");
    }
    let redirect = Url::parse(&redirects[0].1)
        .map_err(|_| anyhow::anyhow!("authentication callback is invalid"))?;
    if redirect.scheme() != "http"
        || !matches!(redirect.host_str(), Some("localhost" | "127.0.0.1" | "::1"))
        || redirect.port().is_none()
        || !redirect.username().is_empty()
        || redirect.password().is_some()
        || redirect.fragment().is_some()
        || redirect.query().is_some()
    {
        anyhow::bail!("authentication callback is invalid");
    }
    Ok((redirect, states[0].1.to_string()))
}

async fn deliver_callback(auth_url: &str, callback_url: &str) -> anyhow::Result<()> {
    let (redirect, expected_state) = validate_auth_url(auth_url)?;
    let callback =
        Url::parse(callback_url).map_err(|_| anyhow::anyhow!("pasted callback URL is invalid"))?;
    if callback.scheme() != redirect.scheme()
        || callback.host_str() != redirect.host_str()
        || callback.port() != redirect.port()
        || callback.path() != redirect.path()
        || !callback.username().is_empty()
        || callback.password().is_some()
        || callback.fragment().is_some()
    {
        anyhow::bail!("pasted callback URL does not match the local login listener");
    }
    let states = callback
        .query_pairs()
        .filter(|(key, _)| key == "state")
        .collect::<Vec<_>>();
    let codes = callback
        .query_pairs()
        .filter(|(key, _)| key == "code")
        .collect::<Vec<_>>();
    let errors = callback
        .query_pairs()
        .filter(|(key, _)| key == "error")
        .collect::<Vec<_>>();
    if states.len() != 1
        || states[0].1 != expected_state
        || (codes.len() == 1) == (errors.len() == 1)
        || codes.len() > 1
        || errors.len() > 1
        || codes.first().is_some_and(|(_, value)| value.is_empty())
        || errors.first().is_some_and(|(_, value)| value.is_empty())
    {
        anyhow::bail!("pasted callback URL has invalid OAuth result fields");
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(CALLBACK_TIMEOUT)
        .build()
        .map_err(|_| anyhow::anyhow!("local callback client is unavailable"))?;
    let response =
        client.get(callback).send().await.map_err(|_| {
            anyhow::anyhow!("could not deliver callback to the local login listener")
        })?;
    match response.status() {
        StatusCode::OK => Ok(()),
        StatusCode::FOUND => {
            let success = response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| Url::parse(value).ok())
                .ok_or_else(|| {
                    anyhow::anyhow!("local login listener returned an invalid redirect")
                })?;
            if success.scheme() != redirect.scheme()
                || success.host_str() != redirect.host_str()
                || success.port() != redirect.port()
                || success.path() != "/success"
                || !success.username().is_empty()
                || success.password().is_some()
                || success.fragment().is_some()
            {
                anyhow::bail!("local login listener returned an invalid redirect");
            }
            let status = client
                .get(success)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("could not finish the local login callback"))?
                .status();
            if status != StatusCode::OK {
                anyhow::bail!("local login listener returned an unexpected status");
            }
            Ok(())
        }
        _ => anyhow::bail!("local login listener returned an unexpected status"),
    }
}

#[cfg(test)]
#[path = "managed_account_lifecycle_tests.rs"]
mod tests;
