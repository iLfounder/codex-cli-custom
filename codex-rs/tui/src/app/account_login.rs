use super::account_picker::AccountLoginMethod;
use super::account_picker::PendingAccountControl;
use super::*;
use crate::app_server_session::cancel_account_login;
use crate::app_server_session::start_account_login as start_default_account_login;
use crate::app_server_session::start_account_slot_login;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotLoginChallenge;
use codex_app_server_protocol::AccountSlotLoginStartParams;
use codex_app_server_protocol::AccountSlotLoginStartResponse;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::CancelLoginAccountParams;
use codex_app_server_protocol::CancelLoginAccountResponse;
use codex_app_server_protocol::CancelLoginAccountStatus;
use codex_app_server_protocol::LoginAccountParams;
use codex_app_server_protocol::LoginAccountResponse;
use codex_app_server_protocol::SessionRuntimeOperation;
use codex_app_server_protocol::SessionRuntimeOperationStatus;

#[derive(Debug)]
pub(crate) enum AccountLoginStartOutcome {
    Default {
        slot_id: String,
        challenge: AccountSlotLoginChallenge,
    },
    Secondary(AccountSlotLoginStartResponse),
}

impl App {
    pub(super) fn start_account_login(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        instance_epoch: String,
        slot_id: Option<String>,
        method: AccountLoginMethod,
    ) {
        let selected_slot = slot_id.as_ref().and_then(|slot_id| {
            self.account_slots
                .iter()
                .find(|slot| slot.account_slot_id == *slot_id)
        });
        if slot_id.is_some() && selected_slot.is_none() {
            self.chat_widget
                .add_error_message("The selected account slot no longer exists.".to_string());
            return;
        }
        if let Some(slot) = selected_slot {
            let action = if matches!(
                slot.status,
                AccountSlotStatus::Ready | AccountSlotStatus::Failed
            ) {
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

        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        let default_slot_id = selected_slot
            .filter(|slot| slot.is_default)
            .map(|slot| slot.account_slot_id.clone());
        tokio::spawn(async move {
            let result = if let Some(default_slot_id) = default_slot_id {
                let params = match method {
                    AccountLoginMethod::Browser => LoginAccountParams::Chatgpt {
                        codex_streamlined_login: false,
                        use_hosted_login_success_page: false,
                        app_brand: None,
                    },
                    AccountLoginMethod::DeviceCode => LoginAccountParams::ChatgptDeviceCode,
                };
                match start_default_account_login(request_handle, params).await {
                    Ok(response) => default_login_outcome(default_slot_id, method, response),
                    Err(error) => Err(error.to_string()),
                }
            } else {
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
                start_account_slot_login(request_handle, params)
                    .await
                    .map(AccountLoginStartOutcome::Secondary)
                    .map_err(|error| error.to_string())
            };
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
        result: Result<AccountLoginStartOutcome, String>,
    ) {
        if self.current_displayed_thread_id() != Some(thread_id) {
            return;
        }
        match result {
            Ok(AccountLoginStartOutcome::Default { slot_id, challenge }) => {
                self.show_account_login_challenge(slot_id, challenge)
            }
            Ok(AccountLoginStartOutcome::Secondary(response)) => {
                if response.operation.action
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
                if let Some(challenge) = response.challenge {
                    self.show_account_login_challenge(
                        response.slot.account_slot_id.clone(),
                        challenge,
                    );
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
                    | SessionRuntimeOperationStatus::Running => {}
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

    fn show_account_login_challenge(
        &mut self,
        slot_id: String,
        challenge: AccountSlotLoginChallenge,
    ) {
        self.chat_widget
            .show_selection_view(login_challenge_params(slot_id, challenge));
    }

    pub(super) fn cancel_account_login(
        &mut self,
        app_server: &AppServerSession,
        slot_id: String,
        login_id: String,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = cancel_account_login(
                request_handle,
                CancelLoginAccountParams {
                    login_id: login_id.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::AccountLoginCanceled {
                slot_id,
                login_id,
                result,
            });
        });
    }

    pub(super) fn handle_account_login_canceled(
        &mut self,
        app_server: &AppServerSession,
        slot_id: &str,
        login_id: &str,
        result: Result<CancelLoginAccountResponse, String>,
    ) {
        if self
            .account_slots
            .iter()
            .find(|slot| slot.account_slot_id == slot_id)
            .and_then(|slot| slot.active_login_operation_id.as_deref())
            .is_some_and(|active_login_id| active_login_id != login_id)
        {
            return;
        }
        match result {
            Ok(response) => match response.status {
                CancelLoginAccountStatus::Canceled | CancelLoginAccountStatus::NotFound => {
                    self.pending_account_control = None;
                    self.refresh_account_state(app_server);
                }
            },
            Err(error) => self
                .chat_widget
                .add_error_message(format!("Could not cancel account login: {error}")),
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

pub(super) fn login_challenge_params(
    slot_id: String,
    challenge: AccountSlotLoginChallenge,
) -> SelectionViewParams {
    let (subtitle, url, login_id) = match challenge {
        AccountSlotLoginChallenge::Browser { login_id, auth_url } => (
            format!("Open this URL to continue:\n{auth_url}"),
            auth_url,
            login_id,
        ),
        AccountSlotLoginChallenge::DeviceCode {
            login_id,
            verification_url,
            user_code,
        } => (
            format!("Open {verification_url}\nand enter code {user_code}"),
            verification_url,
            login_id,
        ),
    };
    SelectionViewParams {
        title: Some("Complete account login".to_string()),
        subtitle: Some(subtitle),
        items: vec![
            SelectionItem {
                name: "Open Browser".to_string(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenUrlInBrowser { url: url.clone() });
                })],
                ..Default::default()
            },
            SelectionItem {
                name: "Cancel login".to_string(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::CancelAccountLogin {
                        slot_id: slot_id.clone(),
                        login_id: login_id.clone(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    }
}

fn default_login_outcome(
    slot_id: String,
    method: AccountLoginMethod,
    response: LoginAccountResponse,
) -> Result<AccountLoginStartOutcome, String> {
    match (method, response) {
        (AccountLoginMethod::Browser, LoginAccountResponse::Chatgpt { login_id, auth_url }) => {
            Ok(AccountLoginStartOutcome::Default {
                slot_id,
                challenge: AccountSlotLoginChallenge::Browser { login_id, auth_url },
            })
        }
        (
            AccountLoginMethod::DeviceCode,
            LoginAccountResponse::ChatgptDeviceCode {
                login_id,
                verification_url,
                user_code,
            },
        ) => Ok(AccountLoginStartOutcome::Default {
            slot_id,
            challenge: AccountSlotLoginChallenge::DeviceCode {
                login_id,
                verification_url,
                user_code,
            },
        }),
        (_, response) => Err(format!(
            "Unexpected account/login/start response: {response:?}"
        )),
    }
}
