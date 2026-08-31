use codex_protocol::ThreadId;
use tempfile::TempDir;

use super::test_support::test_config;
use super::*;

#[tokio::test]
async fn local_transition_uses_state_authority_and_requires_it() {
    let home = TempDir::new().expect("temp dir");
    let config = test_config(home.path());
    let previous =
        ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("previous thread id");
    let current =
        ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("current thread id");
    let intent = ThreadTransitionIntent {
        transition_id: "transition-1".to_string(),
        request_fingerprint: "fingerprint-1".to_string(),
        reason: crate::ThreadTransitionReason::Clear,
        previous_thread_id: previous,
        previous_precondition_state_revision: 4,
    };
    let no_state = LocalThreadStore::new(config.clone(), /*state_db*/ None);
    assert!(matches!(
        no_state
            .claim_thread_transition(
                intent.clone(),
                current,
                "epoch-1".to_string(),
                "client-1".to_string(),
                ThreadWriterEvidence {
                    store_id: "store-1".to_string(),
                    writer_generation: 2,
                },
            )
            .await,
        Err(ThreadStoreError::Unsupported {
            operation: "claim_thread_transition"
        })
    ));

    let runtime = codex_state::StateRuntime::init(
        config.sqlite.clone(),
        config.default_model_provider_id.clone(),
    )
    .await
    .expect("state db should initialize");
    let store = LocalThreadStore::new(config, Some(runtime));
    assert!(matches!(
        store
            .claim_thread_transition(
                intent,
                current,
                "epoch-1".to_string(),
                "client-1".to_string(),
                ThreadWriterEvidence {
                    store_id: "store-1".to_string(),
                    writer_generation: 2,
                },
            )
            .await
            .expect("durable claim should succeed"),
        ThreadTransitionClaimOutcome::NewPreparing(_)
    ));
    assert!(matches!(
        store
            .thread_transition_by_id("transition-1".to_string())
            .await
            .expect("durable read should succeed"),
        Some(ThreadTransitionRecord::Preparing(_))
    ));
    assert_eq!(
        store
            .abort_thread_transition(AbortThreadTransition {
                transition_id: "transition-1".to_string(),
                expected_request_fingerprint: "fingerprint-1".to_string(),
                expected_origin_instance_epoch: "epoch-1".to_string(),
                expected_initiator_client_incarnation: "client-1".to_string(),
                expected_previous_thread_id: previous,
                expected_current_thread_id: current,
            })
            .await
            .expect("durable abort should succeed"),
        ThreadTransitionAbortOutcome::Aborted
    );
    assert_eq!(
        store
            .thread_transition_by_id("transition-1".to_string())
            .await
            .expect("durable read should succeed"),
        None
    );
}
