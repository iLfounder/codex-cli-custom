use pretty_assertions::assert_eq;

use super::*;

fn thread_id(suffix: u128) -> ThreadId {
    ThreadId::from_string(&format!("00000000-0000-0000-0000-{suffix:012x}"))
        .expect("valid thread id")
}

#[tokio::test]
async fn in_memory_transition_matches_claim_prepare_commit_contract() {
    let store = InMemoryThreadStore::default();
    let previous = thread_id(1);
    let current = thread_id(2);
    let intent = ThreadTransitionIntent {
        transition_id: "transition-1".to_string(),
        request_fingerprint: "fingerprint-1".to_string(),
        reason: crate::ThreadTransitionReason::New,
        previous_thread_id: previous,
        previous_precondition_state_revision: 5,
    };
    let previous_writer = ThreadWriterEvidence {
        store_id: "store-1".to_string(),
        writer_generation: 2,
    };
    assert!(matches!(
        store
            .claim_thread_transition(
                intent.clone(),
                current,
                "epoch-1".to_string(),
                "client-1".to_string(),
                previous_writer,
            )
            .await
            .expect("claim should succeed"),
        ThreadTransitionClaimOutcome::NewPreparing(_)
    ));
    let preparation = store
        .mark_thread_transition_prepared(MarkThreadTransitionPrepared {
            transition_id: intent.transition_id.clone(),
            expected_request_fingerprint: intent.request_fingerprint,
            expected_origin_instance_epoch: "epoch-1".to_string(),
            expected_initiator_client_incarnation: "client-1".to_string(),
            current_writer: ThreadWriterEvidence {
                store_id: "store-1".to_string(),
                writer_generation: 1,
            },
        })
        .await
        .expect("prepare should succeed");
    assert_eq!(preparation.preparing.current_thread_id, current);
    let receipt = match store
        .commit_thread_transition(CommitThreadTransition {
            transition_id: intent.transition_id,
            expected_previous_thread_id: previous,
            expected_current_thread_id: current,
            expected_origin_instance_epoch: "epoch-1".to_string(),
            expected_initiator_client_incarnation: "client-1".to_string(),
            previous_committed_state_revision: 8,
            current_committed_state_revision: 3,
        })
        .await
        .expect("commit should succeed")
    {
        ThreadTransitionCommitOutcome::Committed(receipt) => receipt,
        outcome => panic!("unexpected commit outcome {outcome:?}"),
    };
    assert_eq!(receipt.transition_revision, 1);
    assert_eq!(
        store
            .committed_thread_transitions_for_threads(vec![previous, current])
            .await
            .expect("projection should succeed"),
        HashMap::from([
            (
                previous,
                CommittedThreadTransitions {
                    last_incoming: None,
                    last_outgoing: Some(receipt.clone()),
                },
            ),
            (
                current,
                CommittedThreadTransitions {
                    last_incoming: Some(receipt),
                    last_outgoing: None,
                },
            ),
        ])
    );
}

#[tokio::test]
async fn in_memory_exact_abort_allows_a_clean_retry() {
    let store = InMemoryThreadStore::default();
    let previous = thread_id(10);
    let current = thread_id(11);
    let intent = ThreadTransitionIntent {
        transition_id: "transition-abort".to_string(),
        request_fingerprint: "fingerprint-abort".to_string(),
        reason: crate::ThreadTransitionReason::Clear,
        previous_thread_id: previous,
        previous_precondition_state_revision: 5,
    };
    let previous_writer = ThreadWriterEvidence {
        store_id: "store-1".to_string(),
        writer_generation: 2,
    };
    store
        .claim_thread_transition(
            intent.clone(),
            current,
            "epoch-1".to_string(),
            "client-1".to_string(),
            previous_writer.clone(),
        )
        .await
        .expect("claim should succeed");
    let abort = AbortThreadTransition {
        transition_id: intent.transition_id.clone(),
        expected_request_fingerprint: intent.request_fingerprint.clone(),
        expected_origin_instance_epoch: "epoch-1".to_string(),
        expected_initiator_client_incarnation: "client-1".to_string(),
        expected_previous_thread_id: previous,
        expected_current_thread_id: current,
    };
    assert_eq!(
        store
            .abort_thread_transition(abort.clone())
            .await
            .expect("abort should succeed"),
        ThreadTransitionAbortOutcome::Aborted
    );
    assert_eq!(
        store
            .abort_thread_transition(abort)
            .await
            .expect("abort retry should succeed"),
        ThreadTransitionAbortOutcome::AlreadyAbsent
    );
    assert!(matches!(
        store
            .claim_thread_transition(
                intent,
                current,
                "epoch-1".to_string(),
                "client-1".to_string(),
                previous_writer,
            )
            .await
            .expect("claim after abort should succeed"),
        ThreadTransitionClaimOutcome::NewPreparing(_)
    ));
}
