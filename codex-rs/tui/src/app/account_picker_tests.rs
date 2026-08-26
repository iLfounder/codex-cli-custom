use super::*;
use crate::app::test_support::make_test_app;
use codex_app_server_protocol::AccountSlotCapability;
use codex_app_server_protocol::SESSION_RUNTIME_ACCOUNT_ROTATION_CAPABILITY;
use codex_app_server_protocol::SessionRuntimeAccountBinding;
use codex_app_server_protocol::SessionRuntimeAccountSwitchState;
use codex_app_server_protocol::SessionRuntimeCapability;
use codex_app_server_protocol::SessionRuntimeIdentity;
use codex_app_server_protocol::SessionRuntimeLifecycle;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimePersistence;
use codex_app_server_protocol::SessionRuntimePersistenceHealth;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use codex_app_server_protocol::SessionRuntimeWriter;
use codex_app_server_protocol::SessionRuntimeWriterState;
use codex_app_server_protocol::ThreadAccountRotationMode;
use codex_app_server_protocol::ThreadAccountRotationSnapshot;
use pretty_assertions::assert_eq;

fn runtime_snapshot(thread_id: &str, state_revision: u64) -> SessionRuntimeSnapshot {
    SessionRuntimeSnapshot {
        thread_id: thread_id.to_string(),
        state_revision,
        identity: SessionRuntimeIdentity {
            session_id: thread_id.to_string(),
            forked_from_id: None,
            parent_thread_id: None,
            name: None,
            source: "test".to_string(),
            cwd: "/workspace".to_string(),
            git_info: None,
            settings: None,
        },
        lifecycle: SessionRuntimeLifecycle {
            state: SessionRuntimeLifecycleState::Idle,
            active_turn_id: None,
            waiting_on: Vec::new(),
            subscriber_count: 0,
            client_incarnations: Vec::new(),
            last_activity_at: None,
            unload_at: None,
        },
        writer: SessionRuntimeWriter {
            state: SessionRuntimeWriterState::None,
            store_id: None,
            writer_generation: None,
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
            switch_state: SessionRuntimeAccountSwitchState::Stable,
            switch_target_slot_id: None,
            deny_reason: None,
            rotation: None,
        },
        actions: Vec::new(),
        continuity: Default::default(),
    }
}

fn picker_snapshot(
    epoch: &str,
    registry_revision: u64,
    state_revision: u64,
) -> AccountPickerSnapshot {
    AccountPickerSnapshot {
        slots: AccountSlotsSnapshot {
            data: Vec::new(),
            registry_revision,
            multi_account: AccountSlotCapability {
                available: true,
                deny_reason: None,
            },
        },
        runtime: ThreadRuntimeSnapshot {
            instance_epoch: epoch.to_string(),
            snapshot: runtime_snapshot("thread-1", state_revision),
            capabilities: Vec::new(),
        },
    }
}

#[tokio::test]
async fn new_runtime_epoch_accepts_authoritative_revision_reset() {
    let mut app = make_test_app().await;
    app.account_registry_revision = 20;
    app.account_runtime = Some(("epoch-a".to_string(), runtime_snapshot("thread-1", 20)));

    assert_eq!(
        app.apply_account_snapshot(picker_snapshot("epoch-b", 1, 1)),
        true
    );
    assert_eq!(
        (
            app.account_registry_revision,
            app.account_runtime
                .as_ref()
                .map(|(epoch, runtime)| (epoch.as_str(), runtime.state_revision)),
        ),
        (1, Some(("epoch-b", 1)))
    );
}

#[tokio::test]
async fn same_runtime_epoch_rejects_revision_regression() {
    let mut app = make_test_app().await;
    app.account_registry_revision = 20;
    app.account_runtime = Some(("epoch-a".to_string(), runtime_snapshot("thread-1", 20)));

    assert_eq!(
        app.apply_account_snapshot(picker_snapshot("epoch-a", 19, 19)),
        false
    );
    assert_eq!(app.account_registry_revision, 20);
}

#[tokio::test]
async fn same_runtime_epoch_merges_each_fresh_projection_independently() {
    let mut app = make_test_app().await;
    app.account_registry_revision = 20;
    app.account_runtime = Some(("epoch-a".to_string(), runtime_snapshot("thread-1", 20)));

    assert_eq!(
        app.apply_account_snapshot(picker_snapshot("epoch-a", 19, 21)),
        false
    );
    assert_eq!(
        (
            app.account_registry_revision,
            app.account_runtime
                .as_ref()
                .map(|(epoch, runtime)| (epoch.as_str(), runtime.state_revision)),
        ),
        (20, Some(("epoch-a", 21)))
    );
}

#[tokio::test]
async fn rotation_is_exposed_only_with_the_matching_available_capability() {
    let rotation = ThreadAccountRotationSnapshot {
        mode: ThreadAccountRotationMode::Fixed,
        fixed_account_slot_id: None,
        automatic_account_slot_ids: Vec::new(),
        revision: 1,
        last_committed_account_slot_id: None,
    };
    let mut app = make_test_app().await;
    let mut hidden = picker_snapshot("epoch-a", 1, 1);
    hidden.runtime.snapshot.account.rotation = Some(rotation.clone());
    assert_eq!(app.apply_account_snapshot(hidden), true);
    assert_eq!(app.account_rotation_snapshot(), None);

    let mut visible = picker_snapshot("epoch-b", 1, 1);
    visible.runtime.snapshot.account.rotation = Some(rotation.clone());
    visible.runtime.capabilities = vec![SessionRuntimeCapability {
        name: SESSION_RUNTIME_ACCOUNT_ROTATION_CAPABILITY.to_string(),
        available: true,
        deny_reason: None,
    }];
    assert_eq!(app.apply_account_snapshot(visible), true);
    assert_eq!(app.account_rotation_snapshot(), Some(&rotation));
}
