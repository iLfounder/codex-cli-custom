//! Freshly fenced account login, switch, and secondary logout flows.

use super::account_picker::AccountControlIntent;
use super::account_picker::AccountPickerSnapshot;
use super::account_picker::PendingAccountControl;
use super::*;
use crate::app_server_session::list_account_slots;
use crate::app_server_session::logout_account_slot;
use crate::app_server_session::session_runtime_for_thread;
use crate::app_server_session::switch_thread_account;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotLogoutParams;
use codex_app_server_protocol::AccountSlotLogoutResponse;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::SessionRuntimeAccountSwitchState;
use codex_app_server_protocol::SessionRuntimeAction;
use codex_app_server_protocol::SessionRuntimeOperation;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use codex_app_server_protocol::ThreadAccountSwitchParams;

impl App {
    pub(super) fn prepare_account_control(
        &mut self,
        app_server: &AppServerSession,
        intent: AccountControlIntent,
    ) {
        let Some(thread_id) = self.current_displayed_thread_id() else {
            self.chat_widget
                .add_error_message("The displayed session is unavailable.".to_string());
            return;
        };
        let request_generation = self.next_account_request_generation();
        let request_handle = app_server.request_handle();
        let runtime_handle = request_handle.clone();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let (slots, runtime) = tokio::join!(
                list_account_slots(request_handle),
                session_runtime_for_thread(runtime_handle, thread_id)
            );
            let result = match (slots, runtime) {
                (Ok(slots), Ok(runtime)) => Ok(AccountPickerSnapshot { slots, runtime }),
                (Err(error), _) | (_, Err(error)) => Err(error.to_string()),
            };
            app_event_tx.send(AppEvent::AccountControlPrepared {
                thread_id,
                request_generation,
                intent,
                result,
            });
        });
    }

    pub(super) fn handle_account_control_prepared(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        request_generation: u64,
        intent: AccountControlIntent,
        result: Result<AccountPickerSnapshot, String>,
    ) {
        if request_generation != self.account_request_generation
            || self.current_displayed_thread_id() != Some(thread_id)
        {
            return;
        }
        let snapshot = match result {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.chat_widget.add_error_message(format!(
                    "Account state changed before the request: {error}"
                ));
                return;
            }
        };
        self.account_registry_revision = snapshot.slots.registry_revision;
        self.account_slots = snapshot.slots.data;
        self.account_slot_capability = Some(snapshot.slots.multi_account);
        let instance_epoch = snapshot.runtime.instance_epoch;
        let runtime = snapshot.runtime.snapshot;
        self.account_runtime = Some((instance_epoch.clone(), runtime.clone()));
        match intent {
            AccountControlIntent::Login { slot_id, method } => {
                self.start_account_login(app_server, thread_id, instance_epoch, slot_id, method);
            }
            AccountControlIntent::Switch { slot_id } => {
                self.start_account_switch(app_server, thread_id, instance_epoch, runtime, slot_id);
            }
            AccountControlIntent::Logout { slot_id } => {
                self.start_secondary_logout(
                    app_server,
                    thread_id,
                    instance_epoch,
                    runtime,
                    slot_id,
                );
            }
        }
    }

    fn start_account_switch(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        instance_epoch: String,
        runtime: SessionRuntimeSnapshot,
        slot_id: String,
    ) {
        let target_allowed = self.account_slots.iter().any(|slot| {
            slot.account_slot_id == slot_id
                && slot.status == AccountSlotStatus::Ready
                && slot.actions.iter().any(|availability| {
                    availability.action == AccountSlotAction::SwitchTo && availability.allowed
                })
        });
        let runtime_allowed = runtime.lifecycle.active_turn_id.is_none()
            && runtime.account.switch_state == SessionRuntimeAccountSwitchState::Stable
            && runtime.actions.iter().any(|availability| {
                availability.action == SessionRuntimeAction::SwitchAccount && availability.allowed
            });
        let Some(current) = runtime.account.current.as_ref() else {
            self.chat_widget
                .add_error_message("The current account binding is unavailable.".to_string());
            return;
        };
        if !target_allowed || !runtime_allowed || runtime.thread_id != thread_id.to_string() {
            self.chat_widget.add_error_message(
                "Account switching is no longer allowed for this session.".to_string(),
            );
            return;
        }
        let operation_id = format!("tui-account-switch-{}", Uuid::new_v4());
        let params = ThreadAccountSwitchParams {
            operation_id: operation_id.clone(),
            thread_id: thread_id.to_string(),
            target_account_slot_id: slot_id.clone(),
            expected_instance_epoch: instance_epoch.clone(),
            expected_state_revision: runtime.state_revision,
            expected_execution_generation: current.execution_generation,
        };
        self.pending_account_control = Some(PendingAccountControl::Switch {
            operation_id: operation_id.clone(),
            thread_id,
            target_slot_id: slot_id,
            instance_epoch,
            ready_state_revision: None,
            ready_generation: None,
            validation_in_flight: false,
        });
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = switch_thread_account(request_handle, params)
                .await
                .map(|response| response.operation)
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::AccountSwitchFinished {
                operation_id,
                result,
            });
        });
    }

    fn start_secondary_logout(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        instance_epoch: String,
        runtime: SessionRuntimeSnapshot,
        slot_id: String,
    ) {
        let Some(slot) = self
            .account_slots
            .iter()
            .find(|slot| slot.account_slot_id == slot_id)
        else {
            self.chat_widget
                .add_error_message("The selected account slot no longer exists.".to_string());
            return;
        };
        let bound = runtime
            .account
            .current
            .as_ref()
            .is_some_and(|account| account.account_slot_id == slot_id)
            || runtime
                .account
                .active_turn
                .as_ref()
                .is_some_and(|account| account.account_slot_id == slot_id)
            || runtime.account.switch_target_slot_id.as_deref() == Some(slot_id.as_str());
        let allowed = !slot.is_default
            && slot.status == AccountSlotStatus::Ready
            && slot.actions.iter().any(|availability| {
                availability.action == AccountSlotAction::Logout && availability.allowed
            })
            && !bound
            && runtime.thread_id == thread_id.to_string();
        if !allowed {
            self.chat_widget.add_error_message(
                "That account cannot be logged out while it is bound to this session.".to_string(),
            );
            return;
        }
        let prior_generation = slot.attempt_generation;
        let params = AccountSlotLogoutParams {
            account_slot_id: slot_id.clone(),
            expected_registry_revision: self.account_registry_revision,
            expected_attempt_generation: prior_generation,
        };
        self.pending_account_control = Some(PendingAccountControl::Logout {
            thread_id,
            target_slot_id: slot_id.clone(),
            instance_epoch,
            prior_registry_revision: self.account_registry_revision,
            prior_generation,
            validation_in_flight: false,
        });
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = logout_account_slot(request_handle, params)
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::AccountSlotLogoutFinished { slot_id, result });
        });
    }

    pub(super) fn handle_account_switch_operation(
        &mut self,
        app_server: &AppServerSession,
        instance_epoch: Option<&str>,
        operation: &SessionRuntimeOperation,
    ) -> bool {
        let Some(PendingAccountControl::Switch {
            operation_id,
            thread_id,
            target_slot_id,
            instance_epoch: expected_epoch,
            ..
        }) = self.pending_account_control.as_ref()
        else {
            return false;
        };
        if operation.operation_id != *operation_id
            || operation.thread_id.as_deref() != Some(thread_id.to_string().as_str())
            || operation.account_slot_id.as_deref() != Some(target_slot_id.as_str())
            || operation.action
                != codex_app_server_protocol::SessionRuntimeOperationAction::ThreadAccountSwitch
            || instance_epoch.is_some_and(|epoch| epoch != expected_epoch)
        {
            return false;
        }
        match operation.status {
            SessionRuntimeOperationStatus::Ready => {
                self.begin_account_control_validation(
                    app_server,
                    operation.state_revision,
                    operation.execution_generation,
                );
            }
            SessionRuntimeOperationStatus::Failed => {
                let message = operation
                    .error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| "Account switch failed.".to_string());
                self.pending_account_control = None;
                self.chat_widget.add_error_message(message);
            }
            SessionRuntimeOperationStatus::Accepted | SessionRuntimeOperationStatus::Running => {}
            SessionRuntimeOperationStatus::Released => return false,
        }
        true
    }

    pub(super) fn handle_account_switch_finished(
        &mut self,
        app_server: &AppServerSession,
        operation_id: &str,
        result: Result<SessionRuntimeOperation, String>,
    ) {
        let matches = matches!(
            self.pending_account_control.as_ref(),
            Some(PendingAccountControl::Switch { operation_id: pending, .. }) if pending == operation_id
        );
        if !matches {
            return;
        }
        match result {
            Ok(operation) => {
                self.handle_account_switch_operation(
                    app_server, /*instance_epoch*/ None, &operation,
                );
            }
            Err(error) => {
                self.pending_account_control = None;
                self.chat_widget
                    .add_error_message(format!("Account switch failed: {error}"));
            }
        }
    }

    pub(super) fn handle_account_slot_logout_finished(
        &mut self,
        app_server: &AppServerSession,
        slot_id: &str,
        result: Result<AccountSlotLogoutResponse, String>,
    ) {
        let Some(PendingAccountControl::Logout {
            target_slot_id,
            prior_registry_revision,
            prior_generation,
            ..
        }) = self.pending_account_control.as_ref()
        else {
            return;
        };
        if target_slot_id != slot_id {
            return;
        }
        match result {
            Ok(response)
                if response.slot.account_slot_id == slot_id
                    && response.slot.status == AccountSlotStatus::LoginRequired
                    && response.slot.registry_revision
                        == prior_registry_revision.saturating_add(1)
                    && response.slot.attempt_generation == prior_generation.saturating_add(1) =>
            {
                self.begin_account_control_validation(
                    app_server, /*state_revision*/ None, /*execution_generation*/ None,
                );
            }
            Ok(_) => {
                self.pending_account_control = None;
                self.chat_widget.add_error_message(
                    "The logout response did not match the requested account generation."
                        .to_string(),
                );
            }
            Err(error) => {
                self.pending_account_control = None;
                self.chat_widget
                    .add_error_message(format!("Account logout failed: {error}"));
            }
        }
    }

    pub(super) fn begin_account_control_validation(
        &mut self,
        app_server: &AppServerSession,
        state_revision: Option<u64>,
        execution_generation: Option<u64>,
    ) {
        match self.pending_account_control.as_mut() {
            Some(PendingAccountControl::Login {
                validation_in_flight,
                ..
            }) => {
                if *validation_in_flight {
                    return;
                }
                *validation_in_flight = true;
            }
            Some(PendingAccountControl::Switch {
                ready_state_revision,
                ready_generation,
                validation_in_flight,
                ..
            }) => {
                let Some(generation) = execution_generation else {
                    self.pending_account_control = None;
                    self.chat_widget.add_error_message(
                        "The account switch omitted its execution generation.".to_string(),
                    );
                    return;
                };
                let Some(revision) = state_revision else {
                    self.pending_account_control = None;
                    self.chat_widget.add_error_message(
                        "The account switch omitted its state revision.".to_string(),
                    );
                    return;
                };
                if ready_state_revision.is_some_and(|ready| ready != revision) {
                    self.pending_account_control = None;
                    self.chat_widget.add_error_message(
                        "The account switch returned inconsistent revisions.".to_string(),
                    );
                    return;
                }
                if ready_generation.is_some_and(|ready| ready != generation) {
                    self.pending_account_control = None;
                    self.chat_widget.add_error_message(
                        "The account switch returned inconsistent generations.".to_string(),
                    );
                    return;
                }
                *ready_generation = Some(generation);
                *ready_state_revision = Some(revision);
                if *validation_in_flight {
                    return;
                }
                *validation_in_flight = true;
            }
            Some(PendingAccountControl::Logout {
                validation_in_flight,
                ..
            }) => {
                if *validation_in_flight {
                    return;
                }
                *validation_in_flight = true;
            }
            None => return,
        }
        self.open_account_picker(app_server);
    }
}
