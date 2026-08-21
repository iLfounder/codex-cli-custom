use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::SessionRuntimeAccountSwitchState;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimeOperation;
use codex_app_server_protocol::SessionRuntimeOperationAction;
use codex_app_server_protocol::SessionRuntimeOperationError;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_app_server_protocol::SessionRuntimeWriterState;
use codex_app_server_protocol::ThreadAccountSwitchParams;
use codex_app_server_protocol::ThreadAccountSwitchResponse;
use codex_core::execution_account::ExecutionAccountSwitchError;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use sha2::Digest;
use sha2::Sha256;

use super::SessionRuntimeEngine;
use super::unix_timestamp;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::outgoing_message::ConnectionId;

impl SessionRuntimeEngine {
    pub(crate) async fn switch_account(
        &self,
        connection_id: ConnectionId,
        params: ThreadAccountSwitchParams,
    ) -> Result<ThreadAccountSwitchResponse, JSONRPCErrorError> {
        let thread_id = ThreadId::from_string(&params.thread_id)
            .map_err(|_| invalid_params("thread/account/switch threadId is invalid"))?;
        let _thread_list_guard = self
            .thread_list_state_permit
            .acquire()
            .await
            .map_err(|_| internal_error("thread lifecycle coordinator is unavailable"))?;
        let operation = self.begin_operation(switch_operation(&params)).await?;
        if operation.status != SessionRuntimeOperationStatus::Accepted {
            return Ok(ThreadAccountSwitchResponse { operation });
        }

        if params.expected_instance_epoch != self.instance_epoch {
            return self
                .failed_switch(
                    &params.operation_id,
                    "stale_instance_epoch",
                    "server process incarnation changed",
                )
                .await;
        }
        let snapshot = self.refresh_snapshot_for_control(thread_id).await?;
        if snapshot.state_revision != params.expected_state_revision {
            return self
                .failed_switch(
                    &params.operation_id,
                    "stale_state_revision",
                    "thread runtime state changed",
                )
                .await;
        }
        let Some(current) = snapshot.account.current.as_ref() else {
            return self
                .failed_switch(
                    &params.operation_id,
                    "account_unbound",
                    "thread execution account is unavailable",
                )
                .await;
        };
        if current.execution_generation != params.expected_execution_generation {
            return self
                .failed_switch(
                    &params.operation_id,
                    "stale_execution_generation",
                    "thread execution account changed",
                )
                .await;
        }
        let Ok(capability) = self.account_registry.runtime_capability().await else {
            return self
                .failed_switch(
                    &params.operation_id,
                    "multi_account_unavailable",
                    "multi-account execution is unavailable",
                )
                .await;
        };
        if !capability.available
            || snapshot.account.switch_state != SessionRuntimeAccountSwitchState::Stable
        {
            return self
                .failed_switch(
                    &params.operation_id,
                    "multi_account_unavailable",
                    "multi-account execution is unavailable",
                )
                .await;
        }
        if snapshot.writer.state != SessionRuntimeWriterState::OwnedHere {
            return self
                .failed_switch(
                    &params.operation_id,
                    "writer_not_owned",
                    "thread writer is not owned by this process",
                )
                .await;
        }
        if snapshot.writer.store_id.is_none() || snapshot.writer.writer_generation.is_none() {
            return self
                .failed_switch(
                    &params.operation_id,
                    "writer_fence_unavailable",
                    "thread writer authority is unavailable",
                )
                .await;
        }
        if snapshot.lifecycle.state != SessionRuntimeLifecycleState::Idle
            || snapshot.lifecycle.active_turn_id.is_some()
            || !snapshot.lifecycle.waiting_on.is_empty()
        {
            return self
                .failed_switch(
                    &params.operation_id,
                    "thread_not_idle",
                    "thread is not idle",
                )
                .await;
        }
        let (caller_subscribed, subscriber_count) = self
            .thread_state_manager
            .caller_subscription(thread_id, connection_id)
            .await;
        if subscriber_count > 1 || (subscriber_count == 1 && !caller_subscribed) {
            return self
                .failed_switch(
                    &params.operation_id,
                    "other_subscribers_present",
                    "other subscribers are still attached",
                )
                .await;
        }
        if self.thread_manager.get_thread(thread_id).await.is_err() {
            return self
                .failed_switch(
                    &params.operation_id,
                    "thread_not_loaded",
                    "thread runtime is not loaded",
                )
                .await;
        }
        let expected_slot_id = current.account_slot_id.clone();
        let mut operation = self
            .update_operation_status(
                &params.operation_id,
                SessionRuntimeOperationStatus::Running,
                None,
            )
            .await?;
        self.state
            .lock()
            .await
            .switching_accounts
            .insert(thread_id, params.target_account_slot_id.clone());
        self.publish_thread(thread_id).await;

        let switch_result = self
            .thread_manager
            .switch_thread_execution_account(
                thread_id,
                ExecutionAccountBinding {
                    slot_id: expected_slot_id,
                    generation: params.expected_execution_generation,
                },
                params.target_account_slot_id.clone(),
            )
            .await;
        self.state
            .lock()
            .await
            .switching_accounts
            .remove(&thread_id);

        let next = match switch_result {
            Ok(next) => next,
            Err(error) => {
                self.publish_thread(thread_id).await;
                let (code, message) = switch_error(error);
                return self
                    .failed_switch(&params.operation_id, code, message)
                    .await;
            }
        };
        self.publish_thread(thread_id).await;
        operation.status = SessionRuntimeOperationStatus::Ready;
        operation.state_revision = self
            .state
            .lock()
            .await
            .threads
            .get(&thread_id)
            .map(|runtime| runtime.revision);
        operation.execution_generation = Some(next.generation);
        operation.error = None;
        operation.updated_at = unix_timestamp();
        let operation = self.update_operation(operation).await?;
        Ok(ThreadAccountSwitchResponse { operation })
    }

    async fn failed_switch(
        &self,
        operation_id: &str,
        code: &str,
        message: &str,
    ) -> Result<ThreadAccountSwitchResponse, JSONRPCErrorError> {
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
        Ok(ThreadAccountSwitchResponse { operation })
    }
}

fn switch_operation(params: &ThreadAccountSwitchParams) -> SessionRuntimeOperation {
    let fingerprint_input = format!(
        "thread/account/switch:{}:{}:{}:{}:{}",
        params.thread_id,
        params.target_account_slot_id,
        params.expected_instance_epoch,
        params.expected_state_revision,
        params.expected_execution_generation,
    );
    SessionRuntimeOperation {
        operation_id: params.operation_id.clone(),
        request_fingerprint: format!("{:x}", Sha256::digest(fingerprint_input.as_bytes())),
        action: SessionRuntimeOperationAction::ThreadAccountSwitch,
        status: SessionRuntimeOperationStatus::Accepted,
        thread_id: Some(params.thread_id.clone()),
        account_slot_id: Some(params.target_account_slot_id.clone()),
        state_revision: Some(params.expected_state_revision),
        writer_generation: None,
        execution_generation: Some(params.expected_execution_generation),
        error: None,
        updated_at: unix_timestamp(),
    }
}

fn switch_error(error: ExecutionAccountSwitchError) -> (&'static str, &'static str) {
    match error {
        ExecutionAccountSwitchError::TargetUnavailable => (
            "target_account_unavailable",
            "target execution account is unavailable",
        ),
        ExecutionAccountSwitchError::PreparationFailed => (
            "target_runtime_prepare_failed",
            "target execution runtime preparation failed",
        ),
        ExecutionAccountSwitchError::StaleGeneration => (
            "stale_execution_generation",
            "thread execution account changed",
        ),
        ExecutionAccountSwitchError::ThreadBusy => {
            ("thread_not_idle", "thread execution runtime is busy")
        }
        ExecutionAccountSwitchError::PersistenceFailed => (
            "execution_binding_persistence_failed",
            "execution account binding persistence failed",
        ),
    }
}
