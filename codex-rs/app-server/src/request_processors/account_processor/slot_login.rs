use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_app_server_protocol::AccountSlotLoginChallenge;
use codex_app_server_protocol::AccountSlotLoginStartParams;
use codex_app_server_protocol::AccountSlotLoginStartResponse;
use codex_app_server_protocol::ClientResponsePayload;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SessionRuntimeOperation;
use codex_app_server_protocol::SessionRuntimeOperationAction;
use codex_app_server_protocol::SessionRuntimeOperationError;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_login::CodexAuth;
use codex_login::ShutdownHandle;
use codex_login::login_with_api_key;
use codex_models_manager::manager::RefreshStrategy;
use sha2::Digest;
use sha2::Sha256;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::Semaphore;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::ACCOUNT_TOKEN_USAGE_FETCH_TIMEOUT;
use super::AccountRequestProcessor;
use super::ConnectionId;
use super::ConnectionRequestId;
use crate::account_registry::ManifestSlotStatus;
use crate::account_registry::live_registration::ERROR_AUTH_UNAVAILABLE;
use crate::account_registry::live_registration::ERROR_LOGIN_BUSY;
use crate::account_registry::live_registration::ERROR_LOGIN_CANCELED;
use crate::account_registry::live_registration::ERROR_LOGIN_FAILED;
use crate::account_registry::live_registration::ERROR_REFRESH_UNAVAILABLE;
use crate::account_registry::live_registration::PreparedSlotLogin;
use crate::account_registry::live_registration::structured_invalid_request;
use crate::error_code::internal_error;
use crate::external_auth::ExternalAuthBridge;

const SLOT_RETRY_JOIN_TIMEOUT: Duration = Duration::from_secs(3);

mod transport;

pub(super) struct SlotLoginCoordinator {
    active: Mutex<HashMap<String, ActiveSlotLogin>>,
    external_owners: Mutex<HashMap<String, ExternalSlotOwner>>,
    start_lock: Semaphore,
}

impl Default for SlotLoginCoordinator {
    fn default() -> Self {
        Self {
            active: Mutex::new(HashMap::new()),
            external_owners: Mutex::new(HashMap::new()),
            start_lock: Semaphore::new(/*permits*/ 1),
        }
    }
}

#[derive(Clone)]
struct ExternalSlotOwner {
    prepared: PreparedSlotLogin,
    owner_connection: ConnectionId,
    refresh_unavailable: CancellationToken,
}

#[derive(Clone)]
struct ActiveSlotLogin {
    prepared: PreparedSlotLogin,
    owner_connection: ConnectionId,
    cancel: SlotLoginCancel,
    completion: Arc<SlotLoginCompletion>,
}

#[derive(Clone)]
enum SlotLoginCancel {
    Browser(ShutdownHandle),
    Device(CancellationToken),
}

impl SlotLoginCancel {
    fn cancel(&self) {
        match self {
            Self::Browser(handle) => handle.shutdown(),
            Self::Device(token) => token.cancel(),
        }
    }
}

#[derive(Default)]
struct SlotLoginCompletion {
    completed: AtomicBool,
    notify: Notify,
}

impl SlotLoginCompletion {
    async fn wait(&self) {
        let notified = self.notify.notified();
        if !self.completed.load(Ordering::Acquire) {
            notified.await;
        }
    }

    fn complete(&self) {
        self.completed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

impl AccountRequestProcessor {
    pub(crate) async fn login_account_slot(
        &self,
        request_id: ConnectionRequestId,
        params: AccountSlotLoginStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let response = self
            .login_account_slot_response(request_id.connection_id, params)
            .await?;
        Ok(Some(response.into()))
    }

    async fn login_account_slot_response(
        &self,
        owner_connection: ConnectionId,
        params: AccountSlotLoginStartParams,
    ) -> Result<AccountSlotLoginStartResponse, JSONRPCErrorError> {
        let _start = self
            .slot_logins
            .start_lock
            .acquire()
            .await
            .map_err(|_| internal_error("account slot login coordinator is unavailable"))?;
        let (requested_slot, kind) = slot_request_identity(&params);
        if let Some(slot_id) = requested_slot.as_deref() {
            self.await_previous_slot_login(slot_id).await?;
        }

        let operation_id = Uuid::new_v4().to_string();
        let prepared = self
            .account_registry
            .prepare_slot_login(requested_slot, operation_id)
            .await?;
        if let Err(error) = self
            .session_runtime
            .begin_operation(operation(
                &prepared,
                kind,
                SessionRuntimeOperationStatus::Accepted,
                None,
            ))
            .await
        {
            self.finish_slot_failure(&prepared, ERROR_LOGIN_FAILED)
                .await;
            return Err(error);
        }
        match params {
            AccountSlotLoginStartParams::ApiKey { api_key, .. } => {
                let result = login_with_api_key(
                    &prepared.auth_home,
                    &api_key,
                    self.config.cli_auth_credentials_store_mode,
                    self.config.auth_keyring_backend_kind(),
                );
                if result.is_err() {
                    self.finish_slot_failure(&prepared, ERROR_LOGIN_FAILED)
                        .await;
                    return Err(internal_error("failed to save account slot API key"));
                }
                if !self.finish_slot_success(&prepared).await? {
                    return Err(structured_invalid_request(
                        ERROR_LOGIN_CANCELED,
                        "account slot login was superseded",
                    ));
                }
                self.slot_login_response(&prepared, kind, None).await
            }
            AccountSlotLoginStartParams::Chatgpt {
                codex_streamlined_login,
                use_hosted_login_success_page,
                app_brand,
                ..
            } => {
                self.start_slot_browser_login(
                    prepared,
                    owner_connection,
                    codex_streamlined_login,
                    use_hosted_login_success_page,
                    app_brand,
                    kind,
                )
                .await
            }
            AccountSlotLoginStartParams::ChatgptDeviceCode { .. } => {
                self.start_slot_device_login(prepared, owner_connection, kind)
                    .await
            }
            AccountSlotLoginStartParams::ChatgptAuthTokens {
                access_token,
                chatgpt_account_id,
                chatgpt_plan_type,
                ..
            } => {
                if let Some(workspaces) =
                    prepared.runtime.auth_manager.effective_chatgpt_workspaces()
                    && !workspaces.contains(&chatgpt_account_id)
                {
                    self.finish_slot_failure(&prepared, ERROR_LOGIN_FAILED)
                        .await;
                    return Err(structured_invalid_request(
                        ERROR_LOGIN_FAILED,
                        "external auth account is not allowed for this host",
                    ));
                }
                let auth = match CodexAuth::from_external_chatgpt_tokens(
                    &access_token,
                    &chatgpt_account_id,
                    chatgpt_plan_type.as_deref(),
                ) {
                    Ok(auth) => auth,
                    Err(_) => {
                        self.finish_slot_failure(&prepared, ERROR_LOGIN_FAILED)
                            .await;
                        return Err(internal_error("failed to set external account slot auth"));
                    }
                };
                let refresh_unavailable = CancellationToken::new();
                if prepared
                    .runtime
                    .auth_manager
                    .set_external_auth(Arc::new(ExternalAuthBridge::new_for_connection(
                        Arc::clone(&self.outgoing),
                        auth,
                        owner_connection,
                        refresh_unavailable.clone(),
                    )))
                    .await
                    .is_err()
                {
                    self.finish_slot_failure(&prepared, ERROR_LOGIN_FAILED)
                        .await;
                    return Err(internal_error("failed to set external account slot auth"));
                }
                self.slot_logins.external_owners.lock().await.insert(
                    prepared.operation_id.clone(),
                    ExternalSlotOwner {
                        prepared: prepared.clone(),
                        owner_connection,
                        refresh_unavailable: refresh_unavailable.clone(),
                    },
                );
                let ready = self.finish_slot_success(&prepared).await;
                if !matches!(ready, Ok(true)) {
                    self.slot_logins
                        .external_owners
                        .lock()
                        .await
                        .remove(&prepared.operation_id);
                    refresh_unavailable.cancel();
                    return match ready {
                        Ok(false) => Err(structured_invalid_request(
                            ERROR_REFRESH_UNAVAILABLE,
                            "external account owner is unavailable",
                        )),
                        Err(error) => Err(error),
                        Ok(true) => unreachable!("handled above"),
                    };
                }
                let processor = self.clone();
                let operation_id = prepared.operation_id.clone();
                tokio::spawn(async move {
                    refresh_unavailable.cancelled().await;
                    let owner = processor
                        .slot_logins
                        .external_owners
                        .lock()
                        .await
                        .remove(&operation_id);
                    if let Some(owner) = owner {
                        processor.fail_external_slot(&owner.prepared).await;
                    }
                });
                self.slot_login_response(&prepared, kind, None).await
            }
        }
    }

    async fn finish_slot_success(
        &self,
        prepared: &PreparedSlotLogin,
    ) -> Result<bool, JSONRPCErrorError> {
        prepared.runtime.auth_manager.reload().await;
        if prepared.runtime.auth_manager.auth().await.is_none() {
            self.finish_slot_failure(prepared, ERROR_AUTH_UNAVAILABLE)
                .await;
            return Err(structured_invalid_request(
                ERROR_AUTH_UNAVAILABLE,
                "account slot authentication is unavailable",
            ));
        }
        let _ = timeout(
            ACCOUNT_TOKEN_USAGE_FETCH_TIMEOUT,
            prepared
                .runtime
                .models_manager
                .list_models(RefreshStrategy::Online, self.config.http_client_factory()),
        )
        .await;
        if let Some(notification) = self
            .account_registry
            .finish_slot_login(prepared, ManifestSlotStatus::Ready, None)
            .await?
        {
            self.thread_manager.clear_account_plugin_cache(
                &prepared.account_slot_id,
                &prepared.runtime.auth_manager,
            );
            self.outgoing
                .send_server_notification(ServerNotification::AccountSlotChanged(notification))
                .await;
            self.publish_slot_operation(prepared, SessionRuntimeOperationStatus::Ready, None)
                .await;
            return Ok(true);
        }
        Ok(false)
    }

    async fn finish_slot_failure(&self, prepared: &PreparedSlotLogin, error_code: &'static str) {
        if let Ok(Some(notification)) = self
            .account_registry
            .finish_slot_login(prepared, ManifestSlotStatus::Failed, Some(error_code))
            .await
        {
            self.outgoing
                .send_server_notification(ServerNotification::AccountSlotChanged(notification))
                .await;
            self.publish_slot_operation(
                prepared,
                SessionRuntimeOperationStatus::Failed,
                Some(SessionRuntimeOperationError {
                    code: error_code.to_string(),
                    message: "account slot login failed".to_string(),
                }),
            )
            .await;
        }
    }

    async fn fail_external_slot(&self, prepared: &PreparedSlotLogin) {
        let pending_failure = self
            .account_registry
            .finish_slot_login(
                prepared,
                ManifestSlotStatus::Failed,
                Some(ERROR_REFRESH_UNAVAILABLE),
            )
            .await;
        let notification = match pending_failure {
            Ok(Some(notification)) => Some(notification),
            Ok(None) => self
                .account_registry
                .fail_ready_slot(prepared, ERROR_REFRESH_UNAVAILABLE)
                .await
                .ok()
                .flatten(),
            Err(_) => None,
        };
        if let Some(notification) = notification {
            self.outgoing
                .send_server_notification(ServerNotification::AccountSlotChanged(notification))
                .await;
        }
        self.publish_slot_operation(
            prepared,
            SessionRuntimeOperationStatus::Failed,
            Some(SessionRuntimeOperationError {
                code: ERROR_REFRESH_UNAVAILABLE.to_string(),
                message: "external account owner is unavailable".to_string(),
            }),
        )
        .await;
    }

    async fn slot_login_response(
        &self,
        prepared: &PreparedSlotLogin,
        kind: &'static str,
        challenge: Option<AccountSlotLoginChallenge>,
    ) -> Result<AccountSlotLoginStartResponse, JSONRPCErrorError> {
        let slot = self
            .account_registry
            .slot_snapshot(&prepared.account_slot_id)
            .await?;
        let status = if challenge.is_some() {
            SessionRuntimeOperationStatus::Running
        } else {
            SessionRuntimeOperationStatus::Ready
        };
        let operation = self
            .session_runtime
            .update_operation(operation(prepared, kind, status, None))
            .await?;
        Ok(AccountSlotLoginStartResponse {
            operation,
            slot,
            challenge,
        })
    }

    async fn publish_slot_operation(
        &self,
        prepared: &PreparedSlotLogin,
        status: SessionRuntimeOperationStatus,
        error: Option<SessionRuntimeOperationError>,
    ) {
        if let Err(error) = self
            .session_runtime
            .update_operation_status(&prepared.operation_id, status, error)
            .await
        {
            tracing::warn!(
                operation_id = %prepared.operation_id,
                "failed to publish account slot operation state: {}",
                error.message
            );
        }
    }

    async fn await_previous_slot_login(&self, slot_id: &str) -> Result<(), JSONRPCErrorError> {
        let active = self
            .slot_logins
            .active
            .lock()
            .await
            .values()
            .find(|active| active.prepared.account_slot_id == slot_id)
            .cloned();
        let Some(active) = active else {
            return Ok(());
        };
        active.cancel.cancel();
        if timeout(SLOT_RETRY_JOIN_TIMEOUT, active.completion.wait())
            .await
            .is_err()
        {
            return Err(structured_invalid_request(
                ERROR_LOGIN_BUSY,
                "the previous account slot login is still stopping",
            ));
        }
        Ok(())
    }

    async fn complete_active_slot_login(&self, operation_id: &str) {
        self.slot_logins.active.lock().await.remove(operation_id);
    }

    pub(super) async fn cancel_slot_login(
        &self,
        login_id: &str,
    ) -> Result<bool, JSONRPCErrorError> {
        let active = self.slot_logins.active.lock().await.get(login_id).cloned();
        let Some(active) = active else {
            return Ok(false);
        };
        active.cancel.cancel();
        self.finish_slot_failure(&active.prepared, ERROR_LOGIN_CANCELED)
            .await;
        Ok(true)
    }

    pub(super) async fn cancel_all_slot_logins(&self) {
        let active = self
            .slot_logins
            .active
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for login in active {
            login.cancel.cancel();
        }
        let external_owners = self
            .slot_logins
            .external_owners
            .lock()
            .await
            .drain()
            .map(|(_, owner)| owner)
            .collect::<Vec<_>>();
        for owner in external_owners {
            owner.refresh_unavailable.cancel();
            self.fail_external_slot(&owner.prepared).await;
        }
    }

    pub(crate) async fn slot_login_connection_closed(&self, connection_id: ConnectionId) {
        let active = self
            .slot_logins
            .active
            .lock()
            .await
            .values()
            .filter(|active| active.owner_connection == connection_id)
            .cloned()
            .collect::<Vec<_>>();
        for login in active {
            login.cancel.cancel();
            self.finish_slot_failure(&login.prepared, ERROR_REFRESH_UNAVAILABLE)
                .await;
        }
        let external_owners = {
            let mut owners = self.slot_logins.external_owners.lock().await;
            owners
                .extract_if(|_, owner| owner.owner_connection == connection_id)
                .map(|(_, owner)| owner)
                .collect::<Vec<_>>()
        };
        for owner in external_owners {
            owner.refresh_unavailable.cancel();
            self.fail_external_slot(&owner.prepared).await;
        }
    }

    pub(super) async fn forget_external_slot_owner(&self, account_slot_id: &str) {
        let owner = {
            let mut owners = self.slot_logins.external_owners.lock().await;
            let operation_id = owners.iter().find_map(|(operation_id, owner)| {
                (owner.prepared.account_slot_id == account_slot_id).then(|| operation_id.clone())
            });
            operation_id.and_then(|operation_id| owners.remove(&operation_id))
        };
        if let Some(owner) = owner {
            owner.refresh_unavailable.cancel();
        }
    }
}

fn slot_request_identity(params: &AccountSlotLoginStartParams) -> (Option<String>, &'static str) {
    match params {
        AccountSlotLoginStartParams::ApiKey { slot_id, .. } => (slot_id.clone(), "apiKey"),
        AccountSlotLoginStartParams::Chatgpt { slot_id, .. } => (slot_id.clone(), "chatgpt"),
        AccountSlotLoginStartParams::ChatgptDeviceCode { slot_id } => {
            (slot_id.clone(), "chatgptDeviceCode")
        }
        AccountSlotLoginStartParams::ChatgptAuthTokens { slot_id, .. } => {
            (slot_id.clone(), "chatgptAuthTokens")
        }
    }
}

fn operation(
    prepared: &PreparedSlotLogin,
    kind: &str,
    status: SessionRuntimeOperationStatus,
    error: Option<SessionRuntimeOperationError>,
) -> SessionRuntimeOperation {
    let fingerprint_input = format!(
        "accountSlot/login/start:{kind}:{}:{}",
        prepared.account_slot_id, prepared.attempt_generation
    );
    SessionRuntimeOperation {
        operation_id: prepared.operation_id.clone(),
        request_fingerprint: format!("{:x}", Sha256::digest(fingerprint_input.as_bytes())),
        action: SessionRuntimeOperationAction::AccountSlotLogin,
        status,
        thread_id: None,
        account_slot_id: Some(prepared.account_slot_id.clone()),
        state_revision: None,
        writer_generation: None,
        execution_generation: Some(prepared.attempt_generation),
        error,
        updated_at: chrono::Utc::now().timestamp(),
    }
}
