//! Strict release-aware shutdown for the active TUI thread.

use super::*;
use crate::app_server_session::AccountSlotsSnapshot;
use crate::app_server_session::ThreadRuntimeSnapshot;
use crate::app_server_session::list_account_slots;
use crate::app_server_session::relinquish_thread;
use crate::app_server_session::session_runtime_for_thread;
use codex_app_server_protocol::SessionRuntimeAction;
use codex_app_server_protocol::SessionRuntimeOperation;
use codex_app_server_protocol::SessionRuntimeOperationAction;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_app_server_protocol::SessionRuntimeWriterState;
use codex_app_server_protocol::ThreadRelinquishParams;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShutdownIntent {
    Exit,
    LogoutDefault,
}

#[derive(Debug)]
pub(crate) struct ShutdownLookup {
    pub(crate) runtime: ThreadRuntimeSnapshot,
    pub(crate) slots: Option<AccountSlotsSnapshot>,
}

#[derive(Debug)]
pub(super) struct PendingShutdown {
    pub(super) intent: ShutdownIntent,
    pub(super) thread_id: ThreadId,
    pub(super) operation_id: String,
    pub(super) instance_epoch: String,
    pub(super) state_revision: u64,
    pub(super) writer_generation: u64,
    pub(super) released: bool,
    pub(super) thread_closed: bool,
}

impl PendingShutdown {
    fn matches_operation(&self, operation: &SessionRuntimeOperation) -> bool {
        operation.operation_id == self.operation_id
            && operation.thread_id.as_deref() == Some(self.thread_id.to_string().as_str())
            && operation.action == SessionRuntimeOperationAction::ThreadRelinquish
            && operation
                .state_revision
                .is_some_and(|revision| revision >= self.state_revision)
            && operation.writer_generation == Some(self.writer_generation)
    }
}

impl App {
    pub(super) fn begin_release_aware_shutdown(
        &mut self,
        app_server: &AppServerSession,
        intent: ShutdownIntent,
    ) {
        if intent == ShutdownIntent::Exit
            && (self.shutdown_force_exit_armed
                || self.pending_shutdown.is_some()
                || self.shutdown_lookup_in_flight)
        {
            self.shutdown_force_exit_armed = false;
            self.pending_shutdown = None;
            self.shutdown_lookup_in_flight = false;
            self.app_event_tx.send(AppEvent::Exit(ExitMode::Immediate));
            return;
        }
        if self.pending_shutdown.is_some() || self.shutdown_lookup_in_flight {
            return;
        }
        self.shutdown_force_exit_armed = false;
        let Some(thread_id) = self.current_displayed_thread_id() else {
            match intent {
                ShutdownIntent::Exit => self.app_event_tx.send(AppEvent::Exit(ExitMode::Immediate)),
                ShutdownIntent::LogoutDefault => {
                    self.app_event_tx.send(AppEvent::LogoutAfterRelease)
                }
            }
            return;
        };
        self.shutdown_lookup_in_flight = true;
        let request_handle = app_server.request_handle();
        let runtime_handle = request_handle.clone();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let runtime = session_runtime_for_thread(runtime_handle, thread_id).await;
            let result = match intent {
                ShutdownIntent::Exit => runtime.map(|runtime| ShutdownLookup {
                    runtime,
                    slots: None,
                }),
                ShutdownIntent::LogoutDefault => {
                    match (runtime, list_account_slots(request_handle).await) {
                        (Ok(runtime), Ok(slots)) => Ok(ShutdownLookup {
                            runtime,
                            slots: Some(slots),
                        }),
                        (Err(error), _) | (_, Err(error)) => Err(error),
                    }
                }
            }
            .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::ShutdownRuntimeLoaded {
                thread_id,
                intent,
                result,
            });
        });
    }

    pub(super) fn handle_shutdown_runtime_loaded(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        intent: ShutdownIntent,
        result: Result<ShutdownLookup, String>,
    ) {
        self.shutdown_lookup_in_flight = false;
        if self.current_displayed_thread_id() != Some(thread_id) {
            self.shutdown_control_failed(
                "The displayed session changed before release.".to_string(),
            );
            return;
        }
        let ShutdownLookup { runtime, slots } = match result {
            Ok(lookup) => lookup,
            Err(error) => {
                self.shutdown_control_failed(error);
                return;
            }
        };
        let snapshot = runtime.snapshot;
        if snapshot.thread_id != thread_id.to_string() {
            self.shutdown_control_failed(
                "The release snapshot did not match the displayed session.".to_string(),
            );
            return;
        }
        let relinquish = snapshot
            .actions
            .iter()
            .find(|availability| availability.action == SessionRuntimeAction::Relinquish);
        if !relinquish.is_some_and(|availability| availability.allowed)
            || snapshot.writer.state != SessionRuntimeWriterState::OwnedHere
        {
            let reason = relinquish
                .and_then(|availability| availability.deny_reason.clone())
                .or(snapshot.writer.deny_reason)
                .unwrap_or_else(|| "This session cannot release its writer yet.".to_string());
            self.shutdown_control_failed(reason);
            return;
        }
        if intent == ShutdownIntent::LogoutDefault {
            let Some(slots) = slots else {
                self.shutdown_control_failed("The account registry was unavailable.".to_string());
                return;
            };
            let bound_slot = snapshot
                .account
                .current
                .as_ref()
                .map(|account| account.account_slot_id.as_str());
            if !slots
                .data
                .iter()
                .any(|slot| slot.is_default && bound_slot == Some(slot.account_slot_id.as_str()))
            {
                self.shutdown_control_failed(
                    "Switch this session to the default account before using /logout.".to_string(),
                );
                return;
            }
        }
        let Some(writer_generation) = snapshot.writer.writer_generation else {
            self.shutdown_control_failed("The session writer fence is unavailable.".to_string());
            return;
        };
        let operation_id = format!("tui-relinquish-{}", Uuid::new_v4());
        self.pending_shutdown = Some(PendingShutdown {
            intent,
            thread_id,
            operation_id: operation_id.clone(),
            instance_epoch: runtime.instance_epoch.clone(),
            state_revision: snapshot.state_revision,
            writer_generation,
            released: false,
            thread_closed: false,
        });
        let params = ThreadRelinquishParams {
            operation_id: operation_id.clone(),
            thread_id: thread_id.to_string(),
            expected_instance_epoch: runtime.instance_epoch,
            expected_state_revision: snapshot.state_revision,
            expected_writer_generation: writer_generation,
        };
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = relinquish_thread(request_handle, params)
                .await
                .map(|response| response.operation)
                .map_err(|error| error.to_string());
            app_event_tx.send(AppEvent::ShutdownRelinquishFinished {
                operation_id,
                result,
            });
        });
    }

    pub(super) fn handle_shutdown_operation(
        &mut self,
        instance_epoch: Option<&str>,
        operation: &SessionRuntimeOperation,
    ) -> bool {
        let Some(pending) = self.pending_shutdown.as_mut() else {
            return false;
        };
        if !pending.matches_operation(operation)
            || instance_epoch.is_some_and(|epoch| epoch != pending.instance_epoch)
        {
            return false;
        }
        match operation.status {
            SessionRuntimeOperationStatus::Released => pending.released = true,
            SessionRuntimeOperationStatus::Failed => {
                let message = operation
                    .error
                    .as_ref()
                    .map(|error| error.message.clone())
                    .unwrap_or_else(|| "Session release failed.".to_string());
                self.shutdown_control_failed(message);
                return true;
            }
            SessionRuntimeOperationStatus::Accepted | SessionRuntimeOperationStatus::Running => {}
            SessionRuntimeOperationStatus::Ready => return false,
        }
        self.maybe_finish_pending_shutdown();
        true
    }

    pub(super) fn handle_shutdown_relinquish_finished(
        &mut self,
        operation_id: &str,
        result: Result<SessionRuntimeOperation, String>,
    ) {
        if self
            .pending_shutdown
            .as_ref()
            .is_none_or(|pending| pending.operation_id != operation_id)
        {
            return;
        }
        match result {
            Ok(operation) => {
                self.handle_shutdown_operation(/*instance_epoch*/ None, &operation);
            }
            Err(error) => self.shutdown_control_failed(error),
        }
    }

    pub(super) fn handle_pending_shutdown_thread_closed(&mut self, thread_id: &str) -> bool {
        let Some(pending) = self.pending_shutdown.as_mut() else {
            return false;
        };
        if pending.thread_id.to_string() != thread_id {
            return false;
        }
        pending.thread_closed = true;
        self.maybe_finish_pending_shutdown();
        true
    }

    fn maybe_finish_pending_shutdown(&mut self) {
        if !self
            .pending_shutdown
            .as_ref()
            .is_some_and(|pending| pending.released && pending.thread_closed)
        {
            return;
        }
        let Some(pending) = self.pending_shutdown.take() else {
            return;
        };
        self.shutdown_force_exit_armed = false;
        match pending.intent {
            ShutdownIntent::Exit => self.app_event_tx.send(AppEvent::Exit(ExitMode::Immediate)),
            ShutdownIntent::LogoutDefault => self.app_event_tx.send(AppEvent::LogoutAfterRelease),
        }
    }

    fn shutdown_control_failed(&mut self, message: String) {
        self.shutdown_lookup_in_flight = false;
        self.pending_shutdown = None;
        self.shutdown_force_exit_armed = true;
        self.chat_widget.restore_after_shutdown_failure();
        self.chat_widget.add_error_message(format!(
            "Could not safely release this session: {message} Repeat the quit shortcut to exit immediately."
        ));
    }
}

#[cfg(test)]
#[path = "runtime_controls_tests.rs"]
mod tests;
