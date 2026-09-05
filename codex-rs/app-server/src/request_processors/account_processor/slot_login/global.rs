#[cfg(debug_assertions)]
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::sync::Arc;

use codex_app_server_protocol::AccountSlotLoginChallenge;
use codex_app_server_protocol::AccountSlotLoginStartParams;
use codex_app_server_protocol::AccountSlotLoginStartResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::SessionRuntimeOperation;
use codex_app_server_protocol::SessionRuntimeOperationAction;
use codex_app_server_protocol::SessionRuntimeOperationError;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::LoginSuccessPage;
use codex_login::LoginSuccessPageBrand;
use codex_login::ServerOptions as LoginServerOptions;
use codex_login::oauth_client_id;
use codex_login::run_login_server_fail_if_busy;
use sha2::Digest;
use sha2::Sha256;
use tokio::time::timeout;
use uuid::Uuid;

use super::super::LOGIN_CHATGPT_TIMEOUT;
use super::AccountRequestProcessor;
use super::ActiveGlobalLogin;
use super::ConnectionId;
use super::GlobalLoginTerminal;
use super::SLOT_RETRY_JOIN_TIMEOUT;
use super::SlotLoginCancel;
use super::SlotLoginCompletion;
use super::slot_request_identity;
use crate::account_registry::live_registration::ERROR_BROWSER_LOGIN_BUSY;
use crate::account_registry::live_registration::ERROR_LOGIN_BUSY;
use crate::account_registry::live_registration::ERROR_LOGIN_CANCELED;
use crate::account_registry::live_registration::ERROR_LOGIN_FAILED;
use crate::account_registry::live_registration::structured_invalid_request;
use crate::error_code::internal_error;

impl AccountRequestProcessor {
    pub(super) async fn login_global_account_slot_response(
        &self,
        owner_connection: ConnectionId,
        params: AccountSlotLoginStartParams,
    ) -> Result<AccountSlotLoginStartResponse, JSONRPCErrorError> {
        let (account_slot_id, kind) = slot_request_identity(&params);
        let account_slot_id = account_slot_id.ok_or_else(|| {
            structured_invalid_request(
                ERROR_LOGIN_FAILED,
                "managed account must be an exact registered C slot",
            )
        })?;
        self.await_previous_global_login(&account_slot_id).await?;
        let AccountSlotLoginStartParams::Chatgpt {
            codex_streamlined_login,
            use_hosted_login_success_page,
            app_brand,
            ..
        } = &params
        else {
            return Err(structured_invalid_request(
                ERROR_LOGIN_FAILED,
                "managed accounts require browser ChatGPT login",
            ));
        };
        let success_page = if *use_hosted_login_success_page {
            LoginSuccessPage::Hosted {
                url: codex_login::CODEX_OPEN_APP_URL
                    .parse()
                    .map_err(|_| internal_error("invalid Codex open app URL"))?,
                app_brand: match app_brand.unwrap_or_default() {
                    codex_app_server_protocol::LoginAppBrand::Codex => LoginSuccessPageBrand::Codex,
                    codex_app_server_protocol::LoginAppBrand::Chatgpt => {
                        LoginSuccessPageBrand::Chatgpt
                    }
                },
            }
        } else {
            LoginSuccessPage::default()
        };
        #[cfg(debug_assertions)]
        let test_oauth_issuer = test_oauth_issuer_override(
            std::env::var_os("CODEX_APP_SERVER_TEST_OAUTH_ISSUER").as_deref(),
        )
        .map_err(|()| {
            structured_invalid_request(ERROR_LOGIN_FAILED, "test OAuth issuer is invalid")
        })?;

        let lifecycle = self.account_registry.begin_global_login(&params).await?;
        let operation_id = Uuid::new_v4().to_string();
        let operation = global_operation(&operation_id, &account_slot_id, kind);

        let opts = LoginServerOptions {
            open_browser: false,
            codex_streamlined_login: *codex_streamlined_login,
            login_success_page: success_page,
            ..LoginServerOptions::new(
                lifecycle.staging_home().to_path_buf(),
                oauth_client_id(),
                /*forced_chatgpt_workspace_id*/ None,
                AuthCredentialsStoreMode::File,
                self.config.auth_keyring_backend_kind(),
                self.config.auth_route_config(),
            )
        };
        #[cfg(debug_assertions)]
        let opts = {
            let mut opts = opts;
            if let Some(issuer) = test_oauth_issuer {
                opts.issuer = issuer;
            }
            opts
        };
        let server = match run_login_server_fail_if_busy(opts) {
            Ok(server) => server,
            Err(error) => {
                self.account_registry.abort_global_login(lifecycle).await;
                let code = if error.kind() == ErrorKind::AddrInUse {
                    ERROR_BROWSER_LOGIN_BUSY
                } else {
                    ERROR_LOGIN_FAILED
                };
                return Err(structured_invalid_request(
                    code,
                    "browser login callback is unavailable",
                ));
            }
        };

        let challenge = AccountSlotLoginChallenge::Browser {
            login_id: operation_id.clone(),
            auth_url: server.auth_url.clone(),
        };
        let terminal = Arc::new(GlobalLoginTerminal::default());
        let completion = Arc::new(SlotLoginCompletion::default());
        let active = ActiveGlobalLogin {
            account_slot_id: account_slot_id.clone(),
            owner_connection,
            cancel: SlotLoginCancel::Browser(server.cancel_handle()),
            completion: Arc::clone(&completion),
            terminal: Arc::clone(&terminal),
        };
        if !self
            .account_registry
            .set_global_active_login(&account_slot_id, &operation_id)
        {
            active.cancel.cancel();
            self.account_registry.abort_global_login(lifecycle).await;
            return Err(structured_invalid_request(
                ERROR_LOGIN_BUSY,
                "managed account login is already active",
            ));
        }
        self.slot_logins
            .global_active
            .lock()
            .await
            .insert(operation_id.clone(), active);
        if let Err(error) = self.session_runtime.begin_operation(operation).await {
            self.cancel_and_remove_global_login(&operation_id, &account_slot_id)
                .await;
            self.account_registry.abort_global_login(lifecycle).await;
            return Err(error);
        }
        let operation = match self
            .session_runtime
            .update_operation_status(&operation_id, SessionRuntimeOperationStatus::Running, None)
            .await
        {
            Ok(operation) => operation,
            Err(error) => {
                self.cancel_and_remove_global_login(&operation_id, &account_slot_id)
                    .await;
                self.account_registry.abort_global_login(lifecycle).await;
                return Err(error);
            }
        };
        let slot = match self
            .account_registry
            .global_slot_snapshot(&account_slot_id)
            .await
        {
            Ok(slot) => slot,
            Err(error) => {
                self.cancel_and_remove_global_login(&operation_id, &account_slot_id)
                    .await;
                self.account_registry.abort_global_login(lifecycle).await;
                return Err(error);
            }
        };

        let processor = self.clone();
        let task_operation_id = operation_id.clone();
        let task_account_slot_id = account_slot_id.clone();
        tokio::spawn(async move {
            let callback_succeeded = matches!(
                timeout(
                    LOGIN_CHATGPT_TIMEOUT,
                    server.block_until_done_with_callback_result()
                )
                .await,
                Ok(Ok(_))
            );
            let commit_result = if callback_succeeded && terminal.try_begin_commit() {
                Some(
                    processor
                        .account_registry
                        .commit_global_login(lifecycle)
                        .await,
                )
            } else {
                terminal.request_cancel();
                processor
                    .account_registry
                    .abort_global_login(lifecycle)
                    .await;
                None
            };

            processor
                .slot_logins
                .global_active
                .lock()
                .await
                .remove(&task_operation_id);
            processor
                .account_registry
                .clear_global_active_login(&task_account_slot_id, &task_operation_id);
            completion.complete();

            match commit_result {
                Some(Ok(_)) => {
                    processor
                        .account_registry
                        .notify_global_inventory_if_changed(&processor.outgoing)
                        .await;
                    let _ = processor
                        .session_runtime
                        .update_operation_status(
                            &task_operation_id,
                            SessionRuntimeOperationStatus::Ready,
                            None,
                        )
                        .await;
                }
                Some(Err(error)) => {
                    let _ = processor
                        .session_runtime
                        .update_operation_status(
                            &task_operation_id,
                            SessionRuntimeOperationStatus::Failed,
                            Some(SessionRuntimeOperationError {
                                code: ERROR_LOGIN_FAILED.to_string(),
                                message: error.message,
                            }),
                        )
                        .await;
                }
                None => {
                    let _ = processor
                        .session_runtime
                        .update_operation_status(
                            &task_operation_id,
                            SessionRuntimeOperationStatus::Failed,
                            Some(SessionRuntimeOperationError {
                                code: ERROR_LOGIN_CANCELED.to_string(),
                                message: "managed account login failed".to_string(),
                            }),
                        )
                        .await;
                }
            }
        });

        Ok(AccountSlotLoginStartResponse {
            slot,
            operation,
            challenge: Some(challenge),
        })
    }

    async fn await_previous_global_login(
        &self,
        account_slot_id: &str,
    ) -> Result<(), JSONRPCErrorError> {
        let active = self
            .slot_logins
            .global_active
            .lock()
            .await
            .values()
            .find(|active| active.account_slot_id == account_slot_id)
            .cloned();
        let Some(active) = active else {
            return Ok(());
        };
        active.request_cancel();
        timeout(SLOT_RETRY_JOIN_TIMEOUT, active.completion.wait())
            .await
            .map_err(|_| {
                structured_invalid_request(
                    ERROR_LOGIN_BUSY,
                    "the previous managed account login is still stopping",
                )
            })
    }

    async fn cancel_and_remove_global_login(&self, operation_id: &str, account_slot_id: &str) {
        if let Some(active) = self
            .slot_logins
            .global_active
            .lock()
            .await
            .remove(operation_id)
        {
            active.request_cancel();
            active.completion.complete();
        }
        self.account_registry
            .clear_global_active_login(account_slot_id, operation_id);
    }
}

#[cfg(debug_assertions)]
fn test_oauth_issuer_override(raw: Option<&OsStr>) -> Result<Option<String>, ()> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let parsed = url::Url::parse(raw.to_str().ok_or(())?).map_err(|_| ())?;
    let loopback = match parsed.host().ok_or(())? {
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
        url::Host::Domain(_) => false,
    };
    if parsed.scheme() != "http"
        || !loopback
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(());
    }
    Ok(Some(parsed.origin().ascii_serialization()))
}

fn global_operation(
    operation_id: &str,
    account_slot_id: &str,
    kind: &str,
) -> SessionRuntimeOperation {
    let fingerprint = format!("accountSlot/login/start:{kind}:{account_slot_id}");
    SessionRuntimeOperation {
        operation_id: operation_id.to_string(),
        request_fingerprint: format!("{:x}", Sha256::digest(fingerprint.as_bytes())),
        action: SessionRuntimeOperationAction::AccountSlotLogin,
        status: SessionRuntimeOperationStatus::Accepted,
        thread_id: None,
        account_slot_id: Some(account_slot_id.to_string()),
        state_revision: None,
        writer_generation: None,
        execution_generation: Some(0),
        error: None,
        updated_at: chrono::Utc::now().timestamp(),
    }
}

#[cfg(test)]
#[path = "global_tests.rs"]
mod tests;
