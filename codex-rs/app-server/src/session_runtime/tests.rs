use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use codex_app_server_protocol::SessionRuntimeAccountBinding;
use codex_app_server_protocol::SessionRuntimeAccountSwitchState;
use codex_app_server_protocol::SessionRuntimeIdentity;
use codex_app_server_protocol::SessionRuntimeLifecycle;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimeOperation;
use codex_app_server_protocol::SessionRuntimeOperationAction;
use codex_app_server_protocol::SessionRuntimeOperationStatus;
use codex_app_server_protocol::SessionRuntimePersistence;
use codex_app_server_protocol::SessionRuntimePersistenceHealth;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use codex_app_server_protocol::SessionRuntimeWriter;
use codex_app_server_protocol::SessionRuntimeWriterState;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

use super::EngineState;
use super::RuntimeActivity;
use super::SessionRuntimeEngine;
use super::operations::OperationCache;
use super::operations::evict_terminal_operations;
use super::operations::retained_counts;
use super::operations::valid_initial_status;
use super::operations::valid_transition;
use super::pagination::SnapshotCache;
use super::snapshot::RuntimeInventory;
use super::snapshot::RuntimeOverlay;
use super::snapshot::RuntimeRecord;

fn snapshot(thread_id: ThreadId) -> SessionRuntimeSnapshot {
    SessionRuntimeSnapshot {
        thread_id: thread_id.to_string(),
        state_revision: 0,
        continuity: Default::default(),
        identity: SessionRuntimeIdentity {
            session_id: thread_id.to_string(),
            forked_from_id: None,
            parent_thread_id: None,
            name: None,
            source: "test".to_string(),
            cwd: "/sanitized/project".to_string(),
            git_info: None,
            settings: None,
        },
        lifecycle: SessionRuntimeLifecycle {
            state: SessionRuntimeLifecycleState::Idle,
            active_turn_id: None,
            waiting_on: Vec::new(),
            subscriber_count: 1,
            client_incarnations: vec!["opaque-client".to_string()],
            last_activity_at: Some(10),
            unload_at: None,
        },
        writer: SessionRuntimeWriter {
            state: SessionRuntimeWriterState::OwnedHere,
            store_id: Some("opaque-store".to_string()),
            writer_generation: Some(3),
            deny_reason: None,
        },
        persistence: SessionRuntimePersistence {
            jsonl: None,
            sqlite: None,
            lag: None,
            flush_health: SessionRuntimePersistenceHealth::Unknown,
            materialize_health: SessionRuntimePersistenceHealth::Unknown,
            flushed_at: None,
            materialized_at: None,
            deny_reason: None,
        },
        account: SessionRuntimeAccountBinding {
            current: None,
            active_turn: None,
            switch_state: SessionRuntimeAccountSwitchState::Unbound,
            switch_target_slot_id: None,
            deny_reason: Some("account_unbound".to_string()),
        },
        actions: Vec::new(),
    }
}

fn operation(id: usize, status: SessionRuntimeOperationStatus) -> SessionRuntimeOperation {
    SessionRuntimeOperation {
        operation_id: format!("operation-{id}"),
        request_fingerprint: format!("fingerprint-{id}"),
        action: SessionRuntimeOperationAction::ThreadRelinquish,
        status,
        thread_id: Some(ThreadId::default().to_string()),
        account_slot_id: None,
        state_revision: None,
        writer_generation: None,
        execution_generation: None,
        error: None,
        updated_at: 1,
    }
}

fn inventory(thread_ids: &[ThreadId]) -> RuntimeInventory {
    RuntimeInventory {
        records: thread_ids
            .iter()
            .map(|thread_id| RuntimeRecord::LoadedOnly {
                thread_id: *thread_id,
                session_id: thread_id.to_string(),
                source: codex_protocol::protocol::SessionSource::Exec,
                cwd: "/sanitized/project".to_string(),
                forked_from_id: None,
                parent_thread_id: None,
            })
            .collect(),
        overlay: RuntimeOverlay {
            loaded_thread_ids: thread_ids.iter().copied().collect::<HashSet<_>>(),
            account_capability: None,
            switching_accounts: HashMap::new(),
        },
    }
}

fn materialize_plan(plan: &super::pagination::PagePlan) -> Vec<SessionRuntimeSnapshot> {
    plan.records
        .iter()
        .map(|record| snapshot(record.thread_id()))
        .collect()
}

#[test]
fn pagination_cursor_is_stable_and_restarts_after_sequence_change() {
    let thread_ids = (0..3).map(|_index| ThreadId::new()).collect::<Vec<_>>();
    let mut cache = SnapshotCache::default();
    cache.replace(7, 11, inventory(&thread_ids));
    let first_plan = cache
        .plan(None, "epoch-a", 7, 11, 1)
        .expect("first page plan");
    assert_eq!(first_plan.records.len(), 2);
    cache
        .commit(&first_plan, materialize_plan(&first_plan))
        .expect("commit first page");
    let first = cache
        .response(&first_plan, "epoch-a", 1, Vec::new())
        .expect("first page");
    let cursor = first.next_cursor.expect("next cursor");
    let second_plan = cache
        .plan(Some(&cursor), "epoch-a", 7, 11, 1)
        .expect("second page plan");
    assert_eq!(second_plan.records.len(), 1);
    cache
        .commit(&second_plan, materialize_plan(&second_plan))
        .expect("commit second page");
    let second = cache
        .response(&second_plan, "epoch-a", 1, Vec::new())
        .expect("stable second page");

    assert_eq!(first.data.len(), 1);
    assert_eq!(second.data.len(), 1);
    assert!(cache.plan(Some(&cursor), "epoch-a", 8, 11, 1).is_err());
    assert!(cache.plan(Some(&cursor), "epoch-b", 7, 11, 1).is_err());
    cache.replace(7, 11, inventory(&[ThreadId::new()]));
    assert!(cache.plan(Some(&cursor), "epoch-a", 7, 11, 1).is_err());
}

#[test]
fn cursorless_clients_reuse_the_same_snapshot_at_the_same_sequence() {
    let thread_ids = (0..3).map(|_index| ThreadId::new()).collect::<Vec<_>>();
    let mut cache = SnapshotCache::default();
    cache.replace(7, 11, inventory(&thread_ids));
    let first_plan = cache
        .plan(None, "epoch-a", 7, 11, 1)
        .expect("first client plan");
    cache
        .commit(&first_plan, materialize_plan(&first_plan))
        .expect("commit first client page");
    let first = cache
        .response(&first_plan, "epoch-a", 1, Vec::new())
        .expect("first client page");
    let second_plan = cache
        .plan(None, "epoch-a", 7, 11, 2)
        .expect("second client plan");
    cache
        .commit(&second_plan, materialize_plan(&second_plan))
        .expect("commit second client page");
    let second = cache
        .response(&second_plan, "epoch-a", 2, Vec::new())
        .expect("second client page");
    let first_cursor = first.next_cursor.expect("first client cursor");
    let second_cursor = second.next_cursor.expect("second client cursor");

    assert_eq!(first.data.len(), 1);
    assert_eq!(second.data.len(), 2);
    assert_eq!(first.data[0], second.data[0]);
    assert!(cache.plan(Some(&first_cursor), "epoch-a", 7, 11, 1).is_ok());
    assert!(
        cache
            .plan(Some(&second_cursor), "epoch-a", 7, 11, 1)
            .is_ok()
    );
}

#[test]
fn material_changes_advance_only_the_thread_revision() {
    let thread_id = ThreadId::default();
    let mut state = EngineState::default();
    let mut first = snapshot(thread_id);
    SessionRuntimeEngine::apply_runtime_state(&mut state, &mut first, RuntimeActivity::Observe);
    let mut identical = snapshot(thread_id);
    SessionRuntimeEngine::apply_runtime_state(&mut state, &mut identical, RuntimeActivity::Observe);
    let mut changed = snapshot(thread_id);
    changed.writer.writer_generation = Some(4);
    SessionRuntimeEngine::apply_runtime_state(&mut state, &mut changed, RuntimeActivity::Observe);

    assert_eq!((first.state_revision, identical.state_revision), (1, 1));
    assert_eq!(changed.state_revision, 2);
}

#[test]
fn operation_cache_never_evicts_accepted_or_running_entries() {
    let mut cache = OperationCache::default();
    for index in 0..129 {
        let operation = operation(index, SessionRuntimeOperationStatus::Ready);
        cache
            .operations
            .insert(operation.operation_id.clone(), operation);
    }
    cache.terminal_order = (0..129)
        .map(|index| format!("operation-{index}"))
        .collect::<VecDeque<_>>();
    for (index, status) in [
        SessionRuntimeOperationStatus::Accepted,
        SessionRuntimeOperationStatus::Running,
    ]
    .into_iter()
    .enumerate()
    {
        let operation = operation(1000 + index, status);
        cache
            .operations
            .insert(operation.operation_id.clone(), operation);
    }

    evict_terminal_operations(&mut cache);
    let state = EngineState {
        operations: cache,
        ..EngineState::default()
    };

    assert_eq!(retained_counts(&state), (2, 128));
    assert!(state.operations.operations.contains_key("operation-1000"));
    assert!(state.operations.operations.contains_key("operation-1001"));
    assert!(!state.operations.operations.contains_key("operation-0"));
}

#[test]
fn operation_transitions_do_not_regress_terminal_state() {
    assert!(valid_initial_status(
        SessionRuntimeOperationStatus::Accepted
    ));
    assert!(!valid_initial_status(SessionRuntimeOperationStatus::Ready));
    assert!(valid_transition(
        SessionRuntimeOperationStatus::Accepted,
        SessionRuntimeOperationStatus::Running,
    ));
    assert!(valid_transition(
        SessionRuntimeOperationStatus::Running,
        SessionRuntimeOperationStatus::Ready,
    ));
    assert!(!valid_transition(
        SessionRuntimeOperationStatus::Ready,
        SessionRuntimeOperationStatus::Running,
    ));
}
