use codex_app_server_protocol::SessionRuntimeContinuity;
use codex_app_server_protocol::ThreadTransitionEndpointEvidence;
use codex_app_server_protocol::ThreadTransitionReason;
use codex_app_server_protocol::ThreadTransitionReceipt;
use codex_app_server_protocol::ThreadTransitionStatus;
use codex_app_server_protocol::ThreadWriterEvidence;
use codex_protocol::ThreadId;

use super::SessionRuntimeEngine;

impl SessionRuntimeEngine {
    pub(super) async fn continuity_snapshot(
        &self,
        thread_id: ThreadId,
    ) -> SessionRuntimeContinuity {
        let transitions = self
            .thread_store
            .committed_thread_transitions_for_threads(vec![thread_id])
            .await
            .ok()
            .and_then(|mut transitions| transitions.remove(&thread_id))
            .unwrap_or_default();
        SessionRuntimeContinuity {
            last_incoming: transitions.last_incoming.map(api_receipt),
            last_outgoing: transitions.last_outgoing.map(api_receipt),
        }
    }
}

pub(crate) fn api_receipt(
    value: codex_thread_store::ThreadTransitionReceipt,
) -> ThreadTransitionReceipt {
    ThreadTransitionReceipt {
        transition_id: value.transition_id,
        reason: match value.reason {
            codex_thread_store::ThreadTransitionReason::Clear => ThreadTransitionReason::Clear,
            codex_thread_store::ThreadTransitionReason::New => ThreadTransitionReason::New,
        },
        previous: api_endpoint(value.previous),
        current: api_endpoint(value.current),
        origin_instance_epoch: value.origin_instance_epoch,
        initiator_client_incarnation: value.initiator_client_incarnation,
        transition_revision: value.transition_revision,
        committed_at: value.committed_at,
        status: ThreadTransitionStatus::Committed,
    }
}

fn api_endpoint(
    value: codex_thread_store::ThreadTransitionEndpointEvidence,
) -> ThreadTransitionEndpointEvidence {
    ThreadTransitionEndpointEvidence {
        thread_id: value.thread_id.to_string(),
        state_revision: value.state_revision,
        writer: ThreadWriterEvidence {
            store_id: value.writer.store_id,
            writer_generation: value.writer.writer_generation,
        },
    }
}
