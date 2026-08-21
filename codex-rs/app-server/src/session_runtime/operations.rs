use std::collections::HashMap;
use std::collections::VecDeque;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SessionRuntimeOperation;
use codex_app_server_protocol::SessionRuntimeOperationError;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_app_server_protocol::SessionRuntimeOperationUpdatedNotification;

#[cfg(test)]
use super::EngineState;
use super::SessionRuntimeEngine;
use super::unix_timestamp;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;

const MAX_TERMINAL_OPERATIONS: usize = 128;
const MAX_ACTIVE_OPERATIONS: usize = 128;
const MAX_OPERATION_ID_BYTES: usize = 128;

#[derive(Default)]
pub(super) struct OperationCache {
    pub(super) operations: HashMap<String, SessionRuntimeOperation>,
    pub(super) terminal_order: VecDeque<String>,
}

impl SessionRuntimeEngine {
    pub(crate) async fn begin_operation(
        &self,
        operation: SessionRuntimeOperation,
    ) -> Result<SessionRuntimeOperation, JSONRPCErrorError> {
        if operation.operation_id.is_empty()
            || operation.operation_id.len() > MAX_OPERATION_ID_BYTES
        {
            return Err(invalid_params(
                "operationId must contain between 1 and 128 bytes",
            ));
        }
        let _build_guard = self
            .build_lock
            .acquire()
            .await
            .map_err(|_| internal_error("session runtime publisher is unavailable"))?;
        let notification = {
            let mut state = self.state.lock().await;
            if let Some(existing) = state.operations.operations.get(&operation.operation_id) {
                if !same_operation_identity(existing, &operation) {
                    return Err(invalid_params(
                        "operationId was already used for a different request",
                    ));
                }
                return Ok(existing.clone());
            }
            if !valid_initial_status(operation.status) {
                return Err(invalid_params(
                    "new runtime operations must start in accepted status",
                ));
            }
            if state
                .operations
                .operations
                .values()
                .filter(|operation| !is_terminal(operation.status))
                .count()
                >= MAX_ACTIVE_OPERATIONS
            {
                return Err(invalid_params(
                    "too many runtime operations are still active",
                ));
            }
            state.sequence = state.sequence.saturating_add(1);
            state
                .operations
                .operations
                .insert(operation.operation_id.clone(), operation.clone());
            operation_notification(self, state.sequence, operation.clone())
        };
        self.outgoing.send_server_notification(notification).await;
        Ok(operation)
    }

    pub(crate) async fn update_operation(
        &self,
        operation: SessionRuntimeOperation,
    ) -> Result<SessionRuntimeOperation, JSONRPCErrorError> {
        let _build_guard = self
            .build_lock
            .acquire()
            .await
            .map_err(|_| internal_error("session runtime publisher is unavailable"))?;
        let notification = {
            let mut state = self.state.lock().await;
            let Some(existing) = state
                .operations
                .operations
                .get(&operation.operation_id)
                .cloned()
            else {
                return Err(invalid_params(
                    "operationId is not active in this server process",
                ));
            };
            if !same_operation_identity(&existing, &operation) {
                return Err(invalid_params(
                    "operationId was already used for a different request",
                ));
            }
            if existing == operation
                || (existing.status == operation.status && is_terminal(existing.status))
                || !valid_transition(existing.status, operation.status)
            {
                return Ok(existing);
            }
            state.sequence = state.sequence.saturating_add(1);
            if !is_terminal(existing.status) && is_terminal(operation.status) {
                state
                    .operations
                    .terminal_order
                    .push_back(operation.operation_id.clone());
            }
            state
                .operations
                .operations
                .insert(operation.operation_id.clone(), operation.clone());
            evict_terminal_operations(&mut state.operations);
            operation_notification(self, state.sequence, operation.clone())
        };
        self.outgoing.send_server_notification(notification).await;
        Ok(operation)
    }

    pub(crate) async fn update_operation_status(
        &self,
        operation_id: &str,
        status: SessionRuntimeOperationStatus,
        error: Option<SessionRuntimeOperationError>,
    ) -> Result<SessionRuntimeOperation, JSONRPCErrorError> {
        let mut operation = {
            let state = self.state.lock().await;
            state
                .operations
                .operations
                .get(operation_id)
                .cloned()
                .ok_or_else(|| invalid_params("operationId is not active in this server process"))?
        };
        operation.status = status;
        operation.error = error;
        operation.updated_at = unix_timestamp();
        self.update_operation(operation).await
    }
}

fn operation_notification(
    engine: &SessionRuntimeEngine,
    sequence: u64,
    operation: SessionRuntimeOperation,
) -> ServerNotification {
    ServerNotification::SessionRuntimeOperationUpdated(SessionRuntimeOperationUpdatedNotification {
        instance_epoch: engine.instance_epoch.clone(),
        sequence,
        operation,
    })
}

pub(super) fn evict_terminal_operations(cache: &mut OperationCache) {
    while cache.terminal_order.len() > MAX_TERMINAL_OPERATIONS {
        let Some(operation_id) = cache.terminal_order.pop_front() else {
            break;
        };
        if cache
            .operations
            .get(&operation_id)
            .is_some_and(|operation| is_terminal(operation.status))
        {
            cache.operations.remove(&operation_id);
        }
    }
}

pub(super) fn same_operation_identity(
    left: &SessionRuntimeOperation,
    right: &SessionRuntimeOperation,
) -> bool {
    left.operation_id == right.operation_id
        && left.request_fingerprint == right.request_fingerprint
        && left.action == right.action
        && left.thread_id == right.thread_id
        && left.account_slot_id == right.account_slot_id
}

pub(super) fn valid_transition(
    current: SessionRuntimeOperationStatus,
    next: SessionRuntimeOperationStatus,
) -> bool {
    match current {
        SessionRuntimeOperationStatus::Accepted => true,
        SessionRuntimeOperationStatus::Running => next != SessionRuntimeOperationStatus::Accepted,
        SessionRuntimeOperationStatus::Ready => {
            matches!(
                next,
                SessionRuntimeOperationStatus::Ready | SessionRuntimeOperationStatus::Failed
            )
        }
        SessionRuntimeOperationStatus::Released => next == SessionRuntimeOperationStatus::Released,
        SessionRuntimeOperationStatus::Failed => next == SessionRuntimeOperationStatus::Failed,
    }
}

pub(super) fn valid_initial_status(status: SessionRuntimeOperationStatus) -> bool {
    status == SessionRuntimeOperationStatus::Accepted
}

pub(super) fn is_terminal(status: SessionRuntimeOperationStatus) -> bool {
    matches!(
        status,
        SessionRuntimeOperationStatus::Released
            | SessionRuntimeOperationStatus::Ready
            | SessionRuntimeOperationStatus::Failed
    )
}

#[cfg(test)]
pub(super) fn retained_counts(state: &EngineState) -> (usize, usize) {
    let active = state
        .operations
        .operations
        .values()
        .filter(|operation| !is_terminal(operation.status))
        .count();
    (active, state.operations.terminal_order.len())
}
