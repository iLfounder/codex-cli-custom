use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use std::sync::Arc;

use super::*;

fn thread_id(suffix: u128) -> ThreadId {
    ThreadId::from_string(&format!("00000000-0000-0000-0000-{suffix:012x}"))
        .expect("valid thread id")
}

fn intent(transition_id: &str, previous_thread_id: ThreadId) -> ThreadTransitionIntent {
    ThreadTransitionIntent {
        transition_id: transition_id.to_string(),
        request_fingerprint: format!("fingerprint-{transition_id}"),
        reason: ThreadTransitionReason::Clear,
        previous_thread_id,
        previous_precondition_state_revision: 7,
    }
}

fn writer(generation: u64) -> ThreadWriterEvidence {
    ThreadWriterEvidence {
        store_id: "store-1".to_string(),
        writer_generation: generation,
    }
}

async fn runtime() -> (
    Arc<StateRuntime>,
    crate::SqliteConfig,
    scopeguard::ScopeGuard<PathBuf, impl FnOnce(PathBuf)>,
) {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let cleanup = scopeguard::guard(sqlite_home.clone(), |path| {
        let _ = std::fs::remove_dir_all(path);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let runtime = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("state runtime should initialize");
    (runtime, sqlite, cleanup)
}

#[tokio::test]
async fn transition_retry_is_status_specific_and_commit_is_immutable() {
    let (runtime, sqlite, _cleanup) = runtime().await;
    let previous = thread_id(1);
    let current = thread_id(2);
    let intent = intent("transition-1", previous);
    let preparing = match runtime
        .claim_thread_transition(&intent, current, "epoch-1", "client-1", &writer(4))
        .await
        .expect("claim should succeed")
    {
        ThreadTransitionClaimOutcome::NewPreparing(value) => value,
        outcome => panic!("unexpected claim outcome {outcome:?}"),
    };
    assert_eq!(preparing.current_thread_id, current);
    assert_eq!(
        runtime
            .claim_thread_transition(&intent, thread_id(99), "epoch-1", "client-1", &writer(4))
            .await
            .expect("same claim should retry"),
        ThreadTransitionClaimOutcome::ExistingPreparing(preparing.clone())
    );
    let foreign = runtime
        .claim_thread_transition(&intent, current, "epoch-2", "client-2", &writer(4))
        .await
        .expect_err("foreign preparing row must not resume");
    assert_eq!(
        foreign
            .downcast_ref::<ThreadTransitionConflict>()
            .expect("typed conflict")
            .reason(),
        "transition_initiator_mismatch"
    );

    let prepared_request = MarkThreadTransitionPrepared {
        transition_id: intent.transition_id.clone(),
        expected_request_fingerprint: intent.request_fingerprint.clone(),
        expected_origin_instance_epoch: "epoch-1".to_string(),
        expected_initiator_client_incarnation: "client-1".to_string(),
        current_writer: writer(1),
    };
    let preparation = runtime
        .mark_thread_transition_prepared(&prepared_request)
        .await
        .expect("prepare should succeed");
    assert_eq!(
        runtime
            .mark_thread_transition_prepared(&prepared_request)
            .await
            .expect("prepare retry should succeed"),
        preparation
    );
    assert_eq!(
        runtime
            .claim_thread_transition(&intent, current, "epoch-1", "client-1", &writer(4))
            .await
            .expect("prepared claim should retry"),
        ThreadTransitionClaimOutcome::ExistingPrepared(preparation)
    );

    let commit = CommitThreadTransition {
        transition_id: intent.transition_id.clone(),
        expected_previous_thread_id: previous,
        expected_current_thread_id: current,
        expected_origin_instance_epoch: "epoch-1".to_string(),
        expected_initiator_client_incarnation: "client-1".to_string(),
        previous_committed_state_revision: 11,
        current_committed_state_revision: 3,
    };
    let receipt = match runtime
        .commit_thread_transition(&commit)
        .await
        .expect("commit should succeed")
    {
        ThreadTransitionCommitOutcome::Committed(receipt) => receipt,
        outcome => panic!("unexpected commit outcome {outcome:?}"),
    };
    assert_eq!(receipt.previous.state_revision, 11);
    assert_eq!(receipt.current.state_revision, 3);
    let mut later_retry = commit;
    later_retry.previous_committed_state_revision = 20;
    later_retry.current_committed_state_revision = 30;
    assert_eq!(
        runtime
            .commit_thread_transition(&later_retry)
            .await
            .expect("commit retry should succeed"),
        ThreadTransitionCommitOutcome::ExistingCommitted(receipt.clone())
    );
    drop(runtime);
    let runtime = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("state runtime should reopen");
    assert_eq!(
        runtime
            .thread_transition_by_id(&intent.transition_id)
            .await
            .expect("exact read should succeed"),
        Some(ThreadTransitionRecord::Committed(receipt.clone()))
    );
    let projected = runtime
        .committed_thread_transitions_for_threads(&[previous, current, thread_id(3)])
        .await
        .expect("batch projection should succeed");
    assert_eq!(projected[&previous].last_outgoing, Some(receipt.clone()));
    assert_eq!(projected[&current].last_incoming, Some(receipt));
    assert_eq!(
        projected[&thread_id(3)],
        CommittedThreadTransitions::default()
    );
}

#[tokio::test]
async fn only_one_prepared_edge_can_commit_for_an_outgoing_authority() {
    let (runtime, _sqlite, _cleanup) = runtime().await;
    let previous = thread_id(10);
    let first = intent("transition-a", previous);
    let second = intent("transition-b", previous);
    for (intent, current) in [(&first, thread_id(11)), (&second, thread_id(12))] {
        runtime
            .claim_thread_transition(intent, current, "epoch-1", "client-1", &writer(9))
            .await
            .expect("parallel preparation should claim");
        runtime
            .mark_thread_transition_prepared(&MarkThreadTransitionPrepared {
                transition_id: intent.transition_id.clone(),
                expected_request_fingerprint: intent.request_fingerprint.clone(),
                expected_origin_instance_epoch: "epoch-1".to_string(),
                expected_initiator_client_incarnation: "client-1".to_string(),
                current_writer: writer(1),
            })
            .await
            .expect("parallel preparation should prepare");
    }
    runtime
        .commit_thread_transition(&CommitThreadTransition {
            transition_id: first.transition_id,
            expected_previous_thread_id: previous,
            expected_current_thread_id: thread_id(11),
            expected_origin_instance_epoch: "epoch-1".to_string(),
            expected_initiator_client_incarnation: "client-1".to_string(),
            previous_committed_state_revision: 7,
            current_committed_state_revision: 2,
        })
        .await
        .expect("first edge should commit");
    let losing_commit = runtime
        .commit_thread_transition(&CommitThreadTransition {
            transition_id: second.transition_id,
            expected_previous_thread_id: previous,
            expected_current_thread_id: thread_id(12),
            expected_origin_instance_epoch: "epoch-1".to_string(),
            expected_initiator_client_incarnation: "client-1".to_string(),
            previous_committed_state_revision: 7,
            current_committed_state_revision: 2,
        })
        .await
        .expect_err("second edge must lose the outgoing authority");
    assert_eq!(
        losing_commit
            .downcast_ref::<ThreadTransitionConflict>()
            .expect("typed conflict")
            .reason(),
        "outgoing_transition_conflict"
    );
}

#[tokio::test]
async fn exact_abort_removes_only_matching_non_committed_transitions() {
    let (runtime, _sqlite, _cleanup) = runtime().await;
    let previous = thread_id(20);
    let current = thread_id(21);
    let intent = intent("transition-abort", previous);
    runtime
        .claim_thread_transition(&intent, current, "epoch-1", "client-1", &writer(3))
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
    let mut mismatch = abort.clone();
    mismatch.expected_current_thread_id = thread_id(99);
    let error = runtime
        .abort_thread_transition(&mismatch)
        .await
        .expect_err("mismatched abort must fail closed");
    assert_eq!(
        error
            .downcast_ref::<ThreadTransitionConflict>()
            .expect("typed conflict")
            .reason(),
        "transition_abort_mismatch"
    );
    assert!(matches!(
        runtime
            .thread_transition_by_id(&intent.transition_id)
            .await
            .expect("read should succeed"),
        Some(ThreadTransitionRecord::Preparing(_))
    ));
    assert_eq!(
        runtime
            .abort_thread_transition(&abort)
            .await
            .expect("exact abort should succeed"),
        ThreadTransitionAbortOutcome::Aborted
    );
    assert_eq!(
        runtime
            .abort_thread_transition(&abort)
            .await
            .expect("abort retry should observe absence"),
        ThreadTransitionAbortOutcome::AlreadyAbsent
    );

    runtime
        .claim_thread_transition(&intent, current, "epoch-1", "client-1", &writer(3))
        .await
        .expect("claim after abort should recover");
    runtime
        .mark_thread_transition_prepared(&MarkThreadTransitionPrepared {
            transition_id: intent.transition_id.clone(),
            expected_request_fingerprint: intent.request_fingerprint.clone(),
            expected_origin_instance_epoch: "epoch-1".to_string(),
            expected_initiator_client_incarnation: "client-1".to_string(),
            current_writer: writer(1),
        })
        .await
        .expect("prepare should succeed");
    runtime
        .commit_thread_transition(&CommitThreadTransition {
            transition_id: intent.transition_id.clone(),
            expected_previous_thread_id: previous,
            expected_current_thread_id: current,
            expected_origin_instance_epoch: "epoch-1".to_string(),
            expected_initiator_client_incarnation: "client-1".to_string(),
            previous_committed_state_revision: 8,
            current_committed_state_revision: 2,
        })
        .await
        .expect("commit should succeed");
    let error = runtime
        .abort_thread_transition(&abort)
        .await
        .expect_err("committed transition must be immutable");
    assert_eq!(
        error
            .downcast_ref::<ThreadTransitionConflict>()
            .expect("typed conflict")
            .reason(),
        "transition_already_committed"
    );
    assert!(matches!(
        runtime
            .thread_transition_by_id(&intent.transition_id)
            .await
            .expect("read should succeed"),
        Some(ThreadTransitionRecord::Committed(_))
    ));
}
