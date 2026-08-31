use std::io::ErrorKind;
use std::sync::Arc;

use codex_app_server_protocol::AccountSlotLoginChallenge;
use codex_app_server_protocol::AccountSlotLoginStartResponse;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_login::LoginSuccessPage;
use codex_login::LoginSuccessPageBrand;
use codex_login::ServerOptions as LoginServerOptions;
use codex_login::complete_device_code_login;
use codex_login::oauth_client_id;
use codex_login::request_device_code;
use codex_login::run_login_server_fail_if_busy;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::super::LOGIN_CHATGPT_TIMEOUT;
use super::AccountRequestProcessor;
use super::ActiveSlotLogin;
use super::ConnectionId;
use super::ERROR_LOGIN_FAILED;
use super::PreparedSlotLogin;
use super::SlotLoginCancel;
use super::SlotLoginCompletion;
use super::internal_error;
use super::structured_invalid_request;
use crate::account_registry::BrowserLoginOwner;
use crate::account_registry::live_registration::ERROR_BROWSER_LOGIN_BUSY;

impl AccountRequestProcessor {
    pub(super) async fn start_slot_browser_login(
        &self,
        prepared: PreparedSlotLogin,
        owner_connection: ConnectionId,
        codex_streamlined_login: bool,
        use_hosted_login_success_page: bool,
        app_brand: Option<codex_app_server_protocol::LoginAppBrand>,
        kind: &'static str,
    ) -> Result<AccountSlotLoginStartResponse, JSONRPCErrorError> {
        let owner = BrowserLoginOwner::Slot(prepared.operation_id.clone());
        if let Err(error) = self
            .account_registry
            .try_begin_browser_login(owner.clone())
            .await
        {
            self.finish_slot_failure(&prepared, ERROR_BROWSER_LOGIN_BUSY)
                .await;
            return Err(error);
        }
        let success_page = if use_hosted_login_success_page {
            let app_brand = match app_brand.unwrap_or_default() {
                codex_app_server_protocol::LoginAppBrand::Codex => LoginSuccessPageBrand::Codex,
                codex_app_server_protocol::LoginAppBrand::Chatgpt => LoginSuccessPageBrand::Chatgpt,
            };
            LoginSuccessPage::Hosted {
                url: codex_login::CODEX_OPEN_APP_URL
                    .parse()
                    .map_err(|_| internal_error("invalid Codex open app URL"))?,
                app_brand,
            }
        } else {
            LoginSuccessPage::default()
        };
        let opts = self.slot_login_options(&prepared, codex_streamlined_login, success_page);
        let server = match run_login_server_fail_if_busy(opts) {
            Ok(server) => server,
            Err(error) => {
                self.account_registry.finish_browser_login(&owner).await;
                let code = if error.kind() == ErrorKind::AddrInUse {
                    ERROR_BROWSER_LOGIN_BUSY
                } else {
                    ERROR_LOGIN_FAILED
                };
                self.finish_slot_failure(&prepared, code).await;
                return Err(structured_invalid_request(
                    code,
                    "browser login callback is unavailable",
                ));
            }
        };
        let challenge = AccountSlotLoginChallenge::Browser {
            login_id: prepared.operation_id.clone(),
            auth_url: server.auth_url.clone(),
        };
        let completion = Arc::new(SlotLoginCompletion::default());
        let active = ActiveSlotLogin {
            prepared: prepared.clone(),
            owner_connection,
            cancel: SlotLoginCancel::Browser(server.cancel_handle()),
            completion: Arc::clone(&completion),
        };
        self.slot_logins
            .active
            .lock()
            .await
            .insert(prepared.operation_id.clone(), active);
        let started = match self
            .account_registry
            .mark_login_cancelable(
                &prepared.account_slot_id,
                prepared.attempt_generation,
                &prepared.operation_id,
            )
            .await
        {
            Ok(started) => started,
            Err(error) => {
                self.complete_active_slot_login(&prepared.operation_id)
                    .await;
                completion.complete();
                self.account_registry.finish_browser_login(&owner).await;
                self.finish_slot_failure(&prepared, ERROR_LOGIN_FAILED)
                    .await;
                return Err(error);
            }
        };
        self.outgoing
            .send_server_notification(ServerNotification::AccountSlotChanged(started))
            .await;

        let processor = self.clone();
        let operation_id = prepared.operation_id.clone();
        let task_prepared = prepared.clone();
        tokio::spawn(async move {
            let success = matches!(
                timeout(
                    LOGIN_CHATGPT_TIMEOUT,
                    server.block_until_done_with_callback_result()
                )
                .await,
                Ok(Ok(_))
            );
            if success {
                let _ = processor.finish_slot_success(&task_prepared).await;
            } else {
                processor
                    .finish_slot_failure(&task_prepared, ERROR_LOGIN_FAILED)
                    .await;
            }
            processor
                .account_registry
                .finish_browser_login(&owner)
                .await;
            processor.complete_active_slot_login(&operation_id).await;
            completion.complete();
        });

        self.slot_login_response(&prepared, kind, Some(challenge))
            .await
    }

    pub(super) async fn start_slot_device_login(
        &self,
        prepared: PreparedSlotLogin,
        owner_connection: ConnectionId,
        kind: &'static str,
    ) -> Result<AccountSlotLoginStartResponse, JSONRPCErrorError> {
        let opts = self.slot_login_options(
            &prepared,
            /*codex_streamlined_login*/ false,
            LoginSuccessPage::default(),
        );
        let device_code = match request_device_code(&opts).await {
            Ok(device_code) => device_code,
            Err(_) => {
                self.finish_slot_failure(&prepared, ERROR_LOGIN_FAILED)
                    .await;
                return Err(internal_error("failed to request account slot device code"));
            }
        };
        let challenge = AccountSlotLoginChallenge::DeviceCode {
            login_id: prepared.operation_id.clone(),
            verification_url: device_code.verification_url.clone(),
            user_code: device_code.user_code.clone(),
        };
        let cancel = CancellationToken::new();
        let completion = Arc::new(SlotLoginCompletion::default());
        let active = ActiveSlotLogin {
            prepared: prepared.clone(),
            owner_connection,
            cancel: SlotLoginCancel::Device(cancel.clone()),
            completion: Arc::clone(&completion),
        };
        self.slot_logins
            .active
            .lock()
            .await
            .insert(prepared.operation_id.clone(), active);
        let started = match self
            .account_registry
            .mark_login_cancelable(
                &prepared.account_slot_id,
                prepared.attempt_generation,
                &prepared.operation_id,
            )
            .await
        {
            Ok(started) => started,
            Err(error) => {
                self.complete_active_slot_login(&prepared.operation_id)
                    .await;
                completion.complete();
                self.finish_slot_failure(&prepared, ERROR_LOGIN_FAILED)
                    .await;
                return Err(error);
            }
        };
        self.outgoing
            .send_server_notification(ServerNotification::AccountSlotChanged(started))
            .await;

        let processor = self.clone();
        let operation_id = prepared.operation_id.clone();
        let task_prepared = prepared.clone();
        tokio::spawn(async move {
            let success = tokio::select! {
                _ = cancel.cancelled() => false,
                result = complete_device_code_login(opts, device_code) => result.is_ok(),
            };
            if success {
                let _ = processor.finish_slot_success(&task_prepared).await;
            } else {
                processor
                    .finish_slot_failure(&task_prepared, ERROR_LOGIN_FAILED)
                    .await;
            }
            processor.complete_active_slot_login(&operation_id).await;
            completion.complete();
        });

        self.slot_login_response(&prepared, kind, Some(challenge))
            .await
    }

    fn slot_login_options(
        &self,
        prepared: &PreparedSlotLogin,
        codex_streamlined_login: bool,
        login_success_page: LoginSuccessPage,
    ) -> LoginServerOptions {
        LoginServerOptions {
            open_browser: false,
            codex_streamlined_login,
            login_success_page,
            ..LoginServerOptions::new(
                prepared.auth_home.clone(),
                oauth_client_id(),
                prepared.runtime.auth_manager.effective_chatgpt_workspaces(),
                self.config.cli_auth_credentials_store_mode,
                self.config.auth_keyring_backend_kind(),
                self.config.auth_route_config(),
            )
        }
    }
}
