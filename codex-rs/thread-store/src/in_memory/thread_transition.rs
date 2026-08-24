use chrono::Utc;
use codex_protocol::ThreadId;
use std::collections::HashMap;

use super::InMemoryThreadStore;
use super::InMemoryThreadTransition;
use crate::AbortThreadTransition;
use crate::CommitThreadTransition;
use crate::CommittedThreadTransitions;
use crate::MarkThreadTransitionPrepared;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::ThreadTransitionAbortOutcome;
use crate::ThreadTransitionClaimOutcome;
use crate::ThreadTransitionCommitOutcome;
use crate::ThreadTransitionEndpointEvidence;
use crate::ThreadTransitionIntent;
use crate::ThreadTransitionPreparation;
use crate::ThreadTransitionPreparing;
use crate::ThreadTransitionReceipt;
use crate::ThreadTransitionRecord;
use crate::ThreadWriterEvidence;

pub(super) async fn claim(
    store: &InMemoryThreadStore,
    intent: ThreadTransitionIntent,
    reserved_current_thread_id: ThreadId,
    origin_instance_epoch: String,
    initiator_client_incarnation: String,
    previous_writer: ThreadWriterEvidence,
) -> ThreadStoreResult<ThreadTransitionClaimOutcome> {
    if intent.previous_precondition_state_revision == 0
        || previous_writer.writer_generation == 0
        || previous_writer.store_id.is_empty()
    {
        return Err(ThreadStoreError::InvalidRequest {
            message: "thread transition revisions and writer evidence must be valid".to_string(),
        });
    }
    let mut state = store.state.lock().await;
    if let Some(existing) = state.thread_transitions.get(&intent.transition_id) {
        let (reason, previous_thread_id, stored_origin, stored_initiator, stored_writer) =
            match &existing.record {
                ThreadTransitionRecord::Preparing(value) => (
                    value.reason,
                    value.previous_thread_id,
                    &value.origin_instance_epoch,
                    &value.initiator_client_incarnation,
                    &value.previous_writer,
                ),
                ThreadTransitionRecord::Prepared(value) => (
                    value.preparing.reason,
                    value.preparing.previous_thread_id,
                    &value.preparing.origin_instance_epoch,
                    &value.preparing.initiator_client_incarnation,
                    &value.preparing.previous_writer,
                ),
                ThreadTransitionRecord::Committed(value) => (
                    value.reason,
                    value.previous.thread_id,
                    &value.origin_instance_epoch,
                    &value.initiator_client_incarnation,
                    &value.previous.writer,
                ),
            };
        if existing.request_fingerprint != intent.request_fingerprint
            || existing.previous_precondition_state_revision
                != intent.previous_precondition_state_revision
            || reason != intent.reason
            || previous_thread_id != intent.previous_thread_id
            || stored_writer != &previous_writer
        {
            return Err(conflict("transition_id_conflict"));
        }
        if !matches!(existing.record, ThreadTransitionRecord::Committed(_))
            && (stored_origin != &origin_instance_epoch
                || stored_initiator != &initiator_client_incarnation)
        {
            return Err(conflict("transition_initiator_mismatch"));
        }
        return Ok(match &existing.record {
            ThreadTransitionRecord::Preparing(value) => {
                ThreadTransitionClaimOutcome::ExistingPreparing(value.clone())
            }
            ThreadTransitionRecord::Prepared(value) => {
                ThreadTransitionClaimOutcome::ExistingPrepared(value.clone())
            }
            ThreadTransitionRecord::Committed(value) => {
                ThreadTransitionClaimOutcome::ExistingCommitted(value.clone())
            }
        });
    }
    if state
        .thread_transitions
        .values()
        .any(|value| match &value.record {
            ThreadTransitionRecord::Preparing(value) => {
                value.current_thread_id == reserved_current_thread_id
            }
            ThreadTransitionRecord::Prepared(value) => {
                value.preparing.current_thread_id == reserved_current_thread_id
            }
            ThreadTransitionRecord::Committed(value) => {
                value.current.thread_id == reserved_current_thread_id
            }
        })
    {
        return Err(conflict("transition_thread_mismatch"));
    }
    let preparing = ThreadTransitionPreparing {
        transition_id: intent.transition_id.clone(),
        request_fingerprint: intent.request_fingerprint.clone(),
        reason: intent.reason,
        previous_thread_id: intent.previous_thread_id,
        current_thread_id: reserved_current_thread_id,
        origin_instance_epoch,
        initiator_client_incarnation,
        previous_precondition_state_revision: intent.previous_precondition_state_revision,
        previous_writer,
    };
    state.next_thread_transition_revision = state
        .next_thread_transition_revision
        .checked_add(1)
        .ok_or_else(|| ThreadStoreError::Internal {
            message: "thread transition revision overflow".to_string(),
        })?;
    let revision = state.next_thread_transition_revision;
    state.thread_transitions.insert(
        intent.transition_id,
        InMemoryThreadTransition {
            revision,
            request_fingerprint: intent.request_fingerprint,
            previous_precondition_state_revision: intent.previous_precondition_state_revision,
            record: ThreadTransitionRecord::Preparing(preparing.clone()),
        },
    );
    Ok(ThreadTransitionClaimOutcome::NewPreparing(preparing))
}

pub(super) async fn mark_prepared(
    store: &InMemoryThreadStore,
    request: MarkThreadTransitionPrepared,
) -> ThreadStoreResult<ThreadTransitionPreparation> {
    if request.current_writer.writer_generation == 0 || request.current_writer.store_id.is_empty() {
        return Err(ThreadStoreError::InvalidRequest {
            message: "thread transition writer evidence must be valid".to_string(),
        });
    }
    let mut state = store.state.lock().await;
    let existing = state
        .thread_transitions
        .get(&request.transition_id)
        .cloned()
        .ok_or_else(|| conflict("transition_not_prepared"))?;
    if existing.request_fingerprint != request.expected_request_fingerprint {
        return Err(conflict("transition_id_conflict"));
    }
    let (origin_instance_epoch, initiator_client_incarnation) = match &existing.record {
        ThreadTransitionRecord::Preparing(value) => (
            &value.origin_instance_epoch,
            &value.initiator_client_incarnation,
        ),
        ThreadTransitionRecord::Prepared(value) => (
            &value.preparing.origin_instance_epoch,
            &value.preparing.initiator_client_incarnation,
        ),
        ThreadTransitionRecord::Committed(value) => (
            &value.origin_instance_epoch,
            &value.initiator_client_incarnation,
        ),
    };
    if origin_instance_epoch != &request.expected_origin_instance_epoch
        || initiator_client_incarnation != &request.expected_initiator_client_incarnation
    {
        return Err(conflict("transition_initiator_mismatch"));
    }
    let preparing = match existing.record {
        ThreadTransitionRecord::Preparing(value) => value,
        ThreadTransitionRecord::Prepared(value)
            if value.current_writer == request.current_writer =>
        {
            return Ok(value);
        }
        ThreadTransitionRecord::Prepared(_) => return Err(conflict("stale_writer_fence")),
        ThreadTransitionRecord::Committed(_) => {
            return Err(conflict("transition_not_prepared"));
        }
    };
    let preparation = ThreadTransitionPreparation {
        preparing,
        current_writer: request.current_writer,
    };
    let Some(transition) = state.thread_transitions.get_mut(&request.transition_id) else {
        return Err(conflict("transition_not_prepared"));
    };
    transition.record = ThreadTransitionRecord::Prepared(preparation.clone());
    Ok(preparation)
}

pub(super) async fn commit(
    store: &InMemoryThreadStore,
    request: CommitThreadTransition,
) -> ThreadStoreResult<ThreadTransitionCommitOutcome> {
    if request.previous_committed_state_revision == 0
        || request.current_committed_state_revision == 0
    {
        return Err(ThreadStoreError::InvalidRequest {
            message: "thread transition state revisions must be positive".to_string(),
        });
    }
    let mut state = store.state.lock().await;
    let existing = state
        .thread_transitions
        .get(&request.transition_id)
        .cloned()
        .ok_or_else(|| conflict("transition_not_prepared"))?;
    let preparation = match existing.record {
        ThreadTransitionRecord::Preparing(_) => {
            return Err(conflict("transition_not_prepared"));
        }
        ThreadTransitionRecord::Prepared(value) => value,
        ThreadTransitionRecord::Committed(value) => {
            if value.previous.thread_id != request.expected_previous_thread_id
                || value.current.thread_id != request.expected_current_thread_id
            {
                return Err(conflict("transition_thread_mismatch"));
            }
            if value.origin_instance_epoch != request.expected_origin_instance_epoch
                || value.initiator_client_incarnation
                    != request.expected_initiator_client_incarnation
            {
                return Err(conflict("transition_initiator_mismatch"));
            }
            return Ok(ThreadTransitionCommitOutcome::ExistingCommitted(value));
        }
    };
    if preparation.preparing.previous_thread_id != request.expected_previous_thread_id
        || preparation.preparing.current_thread_id != request.expected_current_thread_id
    {
        return Err(conflict("transition_thread_mismatch"));
    }
    if preparation.preparing.origin_instance_epoch != request.expected_origin_instance_epoch
        || preparation.preparing.initiator_client_incarnation
            != request.expected_initiator_client_incarnation
    {
        return Err(conflict("transition_initiator_mismatch"));
    }
    let outgoing_conflict = state.thread_transitions.iter().any(|(id, value)| {
        id != &request.transition_id
            && matches!(
                &value.record,
                ThreadTransitionRecord::Committed(receipt)
                    if receipt.previous.thread_id == preparation.preparing.previous_thread_id
                        && value.previous_precondition_state_revision
                            == preparation.preparing.previous_precondition_state_revision
                        && receipt.previous.writer == preparation.preparing.previous_writer
            )
    });
    if outgoing_conflict {
        return Err(conflict("outgoing_transition_conflict"));
    }
    let receipt = ThreadTransitionReceipt {
        transition_id: request.transition_id.clone(),
        reason: preparation.preparing.reason,
        previous: ThreadTransitionEndpointEvidence {
            thread_id: preparation.preparing.previous_thread_id,
            state_revision: request.previous_committed_state_revision,
            writer: preparation.preparing.previous_writer,
        },
        current: ThreadTransitionEndpointEvidence {
            thread_id: preparation.preparing.current_thread_id,
            state_revision: request.current_committed_state_revision,
            writer: preparation.current_writer,
        },
        origin_instance_epoch: preparation.preparing.origin_instance_epoch,
        initiator_client_incarnation: preparation.preparing.initiator_client_incarnation,
        transition_revision: existing.revision,
        committed_at: Utc::now().timestamp(),
    };
    let Some(transition) = state.thread_transitions.get_mut(&request.transition_id) else {
        return Err(conflict("transition_not_prepared"));
    };
    transition.record = ThreadTransitionRecord::Committed(receipt.clone());
    Ok(ThreadTransitionCommitOutcome::Committed(receipt))
}

pub(super) async fn abort(
    store: &InMemoryThreadStore,
    request: AbortThreadTransition,
) -> ThreadStoreResult<ThreadTransitionAbortOutcome> {
    let mut state = store.state.lock().await;
    let Some(existing) = state.thread_transitions.get(&request.transition_id) else {
        return Ok(ThreadTransitionAbortOutcome::AlreadyAbsent);
    };
    let preparing = match &existing.record {
        ThreadTransitionRecord::Preparing(value) => value,
        ThreadTransitionRecord::Prepared(value) => &value.preparing,
        ThreadTransitionRecord::Committed(_) => {
            return Err(conflict("transition_already_committed"));
        }
    };
    if existing.request_fingerprint != request.expected_request_fingerprint
        || preparing.origin_instance_epoch != request.expected_origin_instance_epoch
        || preparing.initiator_client_incarnation != request.expected_initiator_client_incarnation
        || preparing.previous_thread_id != request.expected_previous_thread_id
        || preparing.current_thread_id != request.expected_current_thread_id
    {
        return Err(conflict("transition_abort_mismatch"));
    }
    state.thread_transitions.remove(&request.transition_id);
    Ok(ThreadTransitionAbortOutcome::Aborted)
}

pub(super) async fn by_id(
    store: &InMemoryThreadStore,
    transition_id: String,
) -> ThreadStoreResult<Option<ThreadTransitionRecord>> {
    Ok(store
        .state
        .lock()
        .await
        .thread_transitions
        .get(&transition_id)
        .map(|value| value.record.clone()))
}

pub(super) async fn committed_for_threads(
    store: &InMemoryThreadStore,
    thread_ids: Vec<ThreadId>,
) -> ThreadStoreResult<HashMap<ThreadId, CommittedThreadTransitions>> {
    let state = store.state.lock().await;
    let mut result = thread_ids
        .into_iter()
        .map(|thread_id| (thread_id, CommittedThreadTransitions::default()))
        .collect::<HashMap<_, _>>();
    let mut receipts = state
        .thread_transitions
        .values()
        .filter_map(|value| match &value.record {
            ThreadTransitionRecord::Committed(receipt) => Some(receipt.clone()),
            ThreadTransitionRecord::Preparing(_) | ThreadTransitionRecord::Prepared(_) => None,
        })
        .collect::<Vec<_>>();
    receipts.sort_by_key(|receipt| std::cmp::Reverse(receipt.transition_revision));
    for receipt in receipts {
        if let Some(continuity) = result.get_mut(&receipt.previous.thread_id)
            && continuity.last_outgoing.is_none()
        {
            continuity.last_outgoing = Some(receipt.clone());
        }
        if let Some(continuity) = result.get_mut(&receipt.current.thread_id)
            && continuity.last_incoming.is_none()
        {
            continuity.last_incoming = Some(receipt);
        }
    }
    Ok(result)
}

fn conflict(reason: &'static str) -> ThreadStoreError {
    ThreadStoreError::Conflict {
        message: reason.to_string(),
    }
}
