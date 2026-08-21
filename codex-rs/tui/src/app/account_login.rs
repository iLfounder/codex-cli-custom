//! Account-slot login transport and terminal validation.

use super::account_picker::AccountLoginMethod;
use super::account_picker::PendingAccountControl;
use super::*;
use crate::app_server_session::start_account_slot_login;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotLoginChallenge;
use codex_app_server_protocol::AccountSlotLoginStartParams;
use codex_app_server_protocol::AccountSlotLoginStartResponse;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::SessionRuntimeOperation;
use codex_app_server_protocol::SessionRuntimeOperationStatus;

impl App {
    pub(super) fn start_account_login(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        instance_epoch: String,
        slot_id: Option<String>,
        method: AccountLoginMethod,
    ) {
        if let Some(slot_id) = slot_id.as_deref() {
            let Some(slot) = self
                .account_slots
                .iter()
                .find(|slot| slot.account_slot_id == slot_id)
            else {
                self.chat_widget
                    .add_error_message("The selected account slot no longer exists.".to_string());
                return;
            };
            let action = if slot.status == AccountSlotStatus::Failed {
                AccountSlotAction::RetryLogin
            } else {
                AccountSlotAction::Login
            };
            if slot.active_login_operation_id.is_some()
                || !slot
                    .actions
                    .iter()
                    .any(|availability| availability.action == action && availability.allowed)
            {
                self.chat_widget.add_error_message(
                    "Login is no longer available for that account.".to_string(),
                );
                return;
            }
        } else if !self
            .account_slot_capability
            .as_ref()
            .is_some_and(|capability| capability.available)
        {
            self.chat_widget
                .add_error_message("Adding another account is unavailable.".to_string());
            return;
        }
        let params = match method {
            AccountLoginMethod::Browser => AccountSlotLoginStartParams::Chatgpt {
                slot_id,
                codex_streamlined_login: false,
                use_hosted_login_success_page: false,
                app_brand: None,
            },
            AccountLoginMethod::DeviceCode => {
                AccountSlotLoginStartParams::ChatgptDeviceCode { slot_id }
            }
        };
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = start_account_slot_login(request_handle, params)
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::AccountSlotLoginStarted {
                thread_id,
                instance_epoch,
                result,
            });
        });
    }

    pub(super) fn handle_account_slot_login_started(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        instance_epoch: String,
        result: Result<AccountSlotLoginStartResponse, String>,
    ) {
        match result {
            Ok(response) => {
                if self.current_displayed_thread_id() != Some(thread_id)
                    || response.operation.action
                        != codex_app_server_protocol::SessionRuntimeOperationAction::AccountSlotLogin
                    || response.operation.operation_id.is_empty()
                    || response.operation.account_slot_id.as_deref()
                        != Some(response.slot.account_slot_id.as_str())
                {
                    self.chat_widget.add_error_message(
                        "The account login response did not match the displayed session."
                            .to_string(),
                    );
                    return;
                }
                self.pending_account_control = Some(PendingAccountControl::Login {
                    operation_id: response.operation.operation_id.clone(),
                    thread_id,
                    target_slot_id: response.slot.account_slot_id.clone(),
                    instance_epoch,
                    attempt_generation: response.slot.attempt_generation,
                    minimum_registry_revision: response.slot.registry_revision,
                    validation_in_flight: false,
                });
                match response.challenge {
                    Some(AccountSlotLoginChallenge::Browser { auth_url, .. }) => {
                        self.open_url_in_browser(auth_url)
                    }
                    Some(AccountSlotLoginChallenge::DeviceCode {
                        verification_url,
                        user_code,
                        ..
                    }) => {
                        self.chat_widget.add_info_message(
                            format!("Open {verification_url} and enter code {user_code}."),
                            /*hint*/ None,
                        );
                    }
                    None => {}
                }
                match response.operation.status {
                    SessionRuntimeOperationStatus::Ready => self.begin_account_control_validation(
                        app_server, /*state_revision*/ None,
                        /*execution_generation*/ None,
                    ),
                    SessionRuntimeOperationStatus::Failed => {
                        self.pending_account_control = None;
                        self.chat_widget.add_error_message(
                            response
                                .operation
                                .error
                                .map(|error| error.message)
                                .unwrap_or_else(|| "Account login failed.".to_string()),
                        );
                    }
                    SessionRuntimeOperationStatus::Accepted
                    | SessionRuntimeOperationStatus::Running => {
                        self.open_account_picker(app_server)
                    }
                    SessionRuntimeOperationStatus::Released => {
                        self.pending_account_control = None;
                        self.chat_widget.add_error_message(
                            "Account login returned an invalid terminal state.".to_string(),
                        );
                    }
                }
            }
            Err(error) => self
                .chat_widget
                .add_error_message(format!("Account login failed: {error}")),
        }
    }

    pub(super) fn handle_account_login_operation(
        &mut self,
        app_server: &AppServerSession,
        instance_epoch: &str,
        operation: &SessionRuntimeOperation,
    ) -> bool {
        let Some(PendingAccountControl::Login {
            operation_id,
            target_slot_id,
            instance_epoch: expected_epoch,
            ..
        }) = self.pending_account_control.as_ref()
        else {
            return false;
        };
        if operation.operation_id != *operation_id
            || operation.account_slot_id.as_deref() != Some(target_slot_id.as_str())
            || operation.action
                != codex_app_server_protocol::SessionRuntimeOperationAction::AccountSlotLogin
            || instance_epoch != expected_epoch
        {
            return false;
        }
        match operation.status {
            SessionRuntimeOperationStatus::Ready => self.begin_account_control_validation(
                app_server, /*state_revision*/ None, /*execution_generation*/ None,
            ),
            SessionRuntimeOperationStatus::Failed => {
                let message = operation
                    .error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| "Account login failed.".to_string());
                self.pending_account_control = None;
                self.chat_widget.add_error_message(message);
            }
            SessionRuntimeOperationStatus::Accepted | SessionRuntimeOperationStatus::Running => {}
            SessionRuntimeOperationStatus::Released => return false,
        }
        true
    }
}
