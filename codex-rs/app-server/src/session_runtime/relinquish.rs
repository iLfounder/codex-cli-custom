use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SessionRuntimeChangedNotification;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimeOperation;
use codex_app_server_protocol::SessionRuntimeOperationAction;
use codex_app_server_protocol::SessionRuntimeOperationError;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_app_server_protocol::SessionRuntimeOperationUpdatedNotification;
use codex_app_server_protocol::SessionRuntimePersistenceHealth;
use codex_app_server_protocol::SessionRuntimeWriterState;
use codex_app_server_protocol::ThreadClosedNotification;
use codex_app_server_protocol::ThreadRelinquishParams;
use codex_app_server_protocol::ThreadRelinquishResponse;
use codex_protocol::ThreadId;
use sha2::Digest;
use sha2::Sha256;

use super::RuntimeActivity;
use super::SessionRuntimeEngine;
use super::operations::evict_terminal_operations;
use super::snapshot::action_snapshot;
use super::snapshot::persistence_snapshot;
use super::snapshot::writer_snapshot;
use super::unix_timestamp;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::outgoing_message::ConnectionId;
use crate::thread_state::RelinquishReservation;

impl SessionRuntimeEngine {
    pub(crate) async fn relinquish(
        &self,
        connection_id: ConnectionId,
        params: ThreadRelinquishParams,
    ) -> Result<ThreadRelinquishResponse, JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|_| invalid_params("thread/relinquish threadId is invalid"))?;
        let _thread_list_guard = self
            .thread_list_state_permit
            .acquire()
            .await
            .map_err(|_| internal_error("thread lifecycle coordinator is unavailable"))?;
        let _transition_admission_permit = self
            .thread_state_manager
            .acquire_thread_mutation_permit(thread_id)
            .await
            .map_err(|reason| invalid_params(reason.replace('_', " ")))?;
        let operation = self.begin_operation(relinquish_operation(&params)).await?;
        if operation.status != SessionRuntimeOperationStatus::Accepted {
            return Ok(ThreadRelinquishResponse { operation });
        }

        if params.expected_instance_epoch != self.instance_epoch {
            return self
                .failed_relinquish(
                    &params.operation_id,
                    "stale_instance_epoch",
                    "server process incarnation changed",
                )
                .await;
        }
        let snapshot = match self.refresh_snapshot_for_control(thread_id).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = self
                    .update_operation_status(
                        &params.operation_id,
                        SessionRuntimeOperationStatus::Failed,
                        Some(SessionRuntimeOperationError {
                            code: "runtime_snapshot_unavailable".to_string(),
                            message: "thread runtime snapshot is unavailable".to_string(),
                        }),
                    )
                    .await;
                return Err(error);
            }
        };
        if snapshot.state_revision != params.expected_state_revision {
            return self
                .failed_relinquish(
                    &params.operation_id,
                    "stale_state_revision",
                    "thread runtime state changed",
                )
                .await;
        }
        if snapshot.writer.writer_generation != Some(params.expected_writer_generation)
            || snapshot.writer.store_id.is_none()
        {
            return self
                .failed_relinquish(
                    &params.operation_id,
                    "stale_writer_fence",
                    "thread writer authority changed",
                )
                .await;
        }
        if snapshot.writer.state != SessionRuntimeWriterState::OwnedHere {
            return self
                .failed_relinquish(
                    &params.operation_id,
                    "writer_not_owned",
                    "thread writer is not owned by this process",
                )
                .await;
        }
        if snapshot.lifecycle.state != SessionRuntimeLifecycleState::Idle
            || snapshot.lifecycle.active_turn_id.is_some()
            || !snapshot.lifecycle.waiting_on.is_empty()
        {
            return self
                .failed_relinquish(
                    &params.operation_id,
                    "thread_not_idle",
                    "thread is not idle",
                )
                .await;
        }
        let Ok(thread) = self.thread_manager.get_thread(thread_id).await else {
            return self
                .failed_relinquish(
                    &params.operation_id,
                    "thread_not_loaded",
                    "thread runtime is not loaded",
                )
                .await;
        };
        match self
            .thread_state_manager
            .reserve_relinquish(
                self.pending_thread_unloads.as_ref(),
                thread_id,
                connection_id,
            )
            .await
        {
            RelinquishReservation::Reserved => {}
            RelinquishReservation::AlreadyClosing => {
                return self
                    .failed_relinquish(
                        &params.operation_id,
                        "thread_closing",
                        "thread is already closing",
                    )
                    .await;
            }
            RelinquishReservation::OtherSubscribersPresent => {
                return self
                    .failed_relinquish(
                        &params.operation_id,
                        "other_subscribers_present",
                        "other subscribers are still attached",
                    )
                    .await;
            }
        }
        if let Err(error) = self
            .update_operation_status(
                params.operation_id.as_str(),
                SessionRuntimeOperationStatus::Running,
                None,
            )
            .await
        {
            self.clear_relinquish_reservation(thread_id).await;
            return Err(error);
        }
        self.publish_thread(thread_id).await;

        if let Err(error) = thread
            .relinquish_and_wait(params.expected_writer_generation)
            .await
        {
            self.clear_relinquish_reservation(thread_id).await;
            self.publish_thread(thread_id).await;
            let (code, message) = if error == "thread is not idle" {
                (
                    "thread_not_idle",
                    "thread became busy before writer release",
                )
            } else {
                (
                    "durability_failed",
                    "strict writer release failed; the thread remains loaded",
                )
            };
            return self
                .failed_relinquish(&params.operation_id, code, message)
                .await;
        }

        // The lifecycle permit and pending-unload reservation prevent app-server resume/replace
        // while this operation runs. If another owner already removed the exact Arc, the strict
        // writer commit is still authoritative: keep the reservation and publish the same terminal
        // pair instead of opening a re-entry window after a committed release.
        let _ = self
            .thread_manager
            .remove_thread_if_matches(&thread_id, &thread)
            .await;
        self.thread_watch_manager
            .remove_thread(&thread_id.to_string())
            .await;
        self.thread_state_manager
            .remove_thread_state(thread_id)
            .await;
        let operation = self
            .publish_relinquish_terminal(thread_id, params.operation_id.as_str())
            .await?;
        Ok(ThreadRelinquishResponse { operation })
    }

    async fn clear_relinquish_reservation(&self, thread_id: ThreadId) {
        self.pending_thread_unloads.lock().await.remove(&thread_id);
    }

    async fn failed_relinquish(
        &self,
        operation_id: &str,
        code: &str,
        message: &str,
    ) -> Result<ThreadRelinquishResponse, JSONRPCErrorError> {
        let operation = self
            .update_operation_status(
                operation_id,
                SessionRuntimeOperationStatus::Failed,
                Some(SessionRuntimeOperationError {
                    code: code.to_string(),
                    message: message.to_string(),
                }),
            )
            .await?;
        Ok(ThreadRelinquishResponse { operation })
    }

    pub(super) async fn refresh_snapshot_for_control(
        &self,
        thread_id: ThreadId,
    ) -> Result<codex_app_server_protocol::SessionRuntimeSnapshot, JSONRPCErrorError> {
        let _build_guard = self
            .build_lock
            .acquire()
            .await
            .map_err(|_| internal_error("session runtime publisher is unavailable"))?;
        let thread_id_text = thread_id.to_string();
        let mut snapshots = self
            .build_consistent_snapshots(Some(thread_id_text.as_str()))
            .await?;
        let Some(mut snapshot) = snapshots.pop() else {
            return Err(invalid_params("thread runtime was not found"));
        };
        let notification = {
            let mut state = self.state.lock().await;
            let changed =
                Self::apply_runtime_state(&mut state, &mut snapshot, RuntimeActivity::Observe);
            changed.then(|| {
                state.sequence = state.sequence.saturating_add(1);
                state.pages.clear();
                SessionRuntimeChangedNotification {
                    instance_epoch: self.instance_epoch.clone(),
                    sequence: state.sequence,
                    snapshot: snapshot.clone(),
                }
            })
        };
        if let Some(notification) = notification {
            self.outgoing
                .send_server_notification(ServerNotification::SessionRuntimeChanged(notification))
                .await;
        }
        Ok(snapshot)
    }

    async fn publish_relinquish_terminal(
        &self,
        thread_id: ThreadId,
        operation_id: &str,
    ) -> Result<SessionRuntimeOperation, JSONRPCErrorError> {
        let _build_guard = self
            .build_lock
            .acquire()
            .await
            .map_err(|_| internal_error("session runtime publisher is unavailable"))?;
        let store_runtime = self.thread_store.runtime_snapshot(thread_id).await;
        let multi_account = self.account_registry.runtime_capability().await.ok();
        let (runtime_notification, operation_notification, operation) = {
            let mut state = self.state.lock().await;
            let mut snapshot = state
                .threads
                .get(&thread_id)
                .and_then(|runtime| runtime.last_snapshot.clone())
                .ok_or_else(|| internal_error("released thread snapshot disappeared"))?;
            snapshot.lifecycle.state = SessionRuntimeLifecycleState::NotLoaded;
            snapshot.lifecycle.active_turn_id = None;
            snapshot.lifecycle.waiting_on.clear();
            snapshot.lifecycle.subscriber_count = 0;
            snapshot.lifecycle.client_incarnations.clear();
            snapshot.lifecycle.unload_at = None;
            match store_runtime {
                Ok(store_runtime) => {
                    snapshot.writer = writer_snapshot(&store_runtime);
                    snapshot.persistence = persistence_snapshot(&store_runtime);
                }
                Err(_) => {
                    snapshot.writer.state = SessionRuntimeWriterState::None;
                    snapshot.writer.deny_reason = None;
                    snapshot.persistence.flush_health = SessionRuntimePersistenceHealth::Unknown;
                    snapshot.persistence.materialize_health =
                        SessionRuntimePersistenceHealth::Unknown;
                    snapshot.persistence.deny_reason =
                        Some("released_persistence_snapshot_unavailable".to_string());
                }
            }
            snapshot.actions = action_snapshot(
                &snapshot.lifecycle,
                &snapshot.writer,
                &snapshot.account,
                multi_account.as_ref(),
            );
            Self::apply_runtime_state(&mut state, &mut snapshot, RuntimeActivity::Activity);
            state.sequence = state.sequence.saturating_add(1);
            state.pages.clear();
            let runtime_notification = SessionRuntimeChangedNotification {
                instance_epoch: self.instance_epoch.clone(),
                sequence: state.sequence,
                snapshot,
            };

            let mut operation = state
                .operations
                .operations
                .get(operation_id)
                .cloned()
                .ok_or_else(|| internal_error("relinquish operation cache entry disappeared"))?;
            operation.status = SessionRuntimeOperationStatus::Released;
            operation.state_revision = Some(runtime_notification.snapshot.state_revision);
            operation.error = None;
            operation.updated_at = unix_timestamp();
            state.sequence = state.sequence.saturating_add(1);
            state.pages.clear();
            state
                .operations
                .terminal_order
                .push_back(operation.operation_id.clone());
            state
                .operations
                .operations
                .insert(operation.operation_id.clone(), operation.clone());
            evict_terminal_operations(&mut state.operations);
            let operation_notification = SessionRuntimeOperationUpdatedNotification {
                instance_epoch: self.instance_epoch.clone(),
                sequence: state.sequence,
                operation: operation.clone(),
            };
            (runtime_notification, operation_notification, operation)
        };
        self.outgoing
            .send_server_notification(ServerNotification::SessionRuntimeChanged(
                runtime_notification,
            ))
            .await;
        self.outgoing
            .send_server_notification(ServerNotification::SessionRuntimeOperationUpdated(
                operation_notification,
            ))
            .await;
        self.outgoing
            .send_server_notification(ServerNotification::ThreadClosed(ThreadClosedNotification {
                thread_id: thread_id.to_string(),
            }))
            .await;
        self.clear_relinquish_reservation(thread_id).await;
        Ok(operation)
    }
}

fn relinquish_operation(params: &ThreadRelinquishParams) -> SessionRuntimeOperation {
    let fingerprint_input = format!(
        "thread/relinquish:{}:{}:{}:{}",
        params.thread_id,
        params.expected_instance_epoch,
        params.expected_state_revision,
        params.expected_writer_generation,
    );
    SessionRuntimeOperation {
        operation_id: params.operation_id.clone(),
        request_fingerprint: format!("{:x}", Sha256::digest(fingerprint_input.as_bytes())),
        action: SessionRuntimeOperationAction::ThreadRelinquish,
        status: SessionRuntimeOperationStatus::Accepted,
        thread_id: Some(params.thread_id.clone()),
        account_slot_id: None,
        state_revision: Some(params.expected_state_revision),
        writer_generation: Some(params.expected_writer_generation),
        execution_generation: None,
        error: None,
        updated_at: unix_timestamp(),
    }
}
