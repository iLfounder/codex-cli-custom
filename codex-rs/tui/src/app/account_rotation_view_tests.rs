use super::*;
use crate::app::test_support::make_test_app;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::SessionRuntimeAccountBinding;
use codex_app_server_protocol::SessionRuntimeAccountRef;
use codex_app_server_protocol::SessionRuntimeAccountSwitchState;
use codex_app_server_protocol::SessionRuntimeIdentity;
use codex_app_server_protocol::SessionRuntimeLifecycle;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimePersistence;
use codex_app_server_protocol::SessionRuntimePersistenceHealth;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use codex_app_server_protocol::SessionRuntimeWriter;
use codex_app_server_protocol::SessionRuntimeWriterState;
use codex_app_server_protocol::ThreadAccountRotationSnapshot;
use insta::assert_snapshot;

fn slot(
    account_number: u32,
    id: &str,
    status: AccountSlotStatus,
    active_login: Option<&str>,
) -> AccountSlotSnapshot {
    AccountSlotSnapshot {
        account_slot_id: id.to_string(),
        account_number,
        label: id.to_string(),
        is_default: account_number == 1,
        status,
        auth_mode: None,
        attempt_generation: 1,
        registry_revision: 9,
        active_login_operation_id: active_login.map(str::to_string),
        error_code: (status == AccountSlotStatus::Failed).then(|| "refreshUnavailable".to_string()),
        actions: Vec::new(),
        updated_at: 0,
    }
}

fn runtime(rotation: ThreadAccountRotationSnapshot) -> SessionRuntimeSnapshot {
    SessionRuntimeSnapshot {
        thread_id: "thread-1".to_string(),
        state_revision: 12,
        identity: SessionRuntimeIdentity {
            session_id: "thread-1".to_string(),
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
            current: Some(SessionRuntimeAccountRef {
                account_slot_id: "default".to_string(),
                execution_generation: 2,
            }),
            active_turn: None,
            rotation: Some(rotation),
            switch_state: SessionRuntimeAccountSwitchState::Stable,
            switch_target_slot_id: None,
            deny_reason: None,
        },
        actions: Vec::new(),
        continuity: Default::default(),
    }
}

fn rendered_items(params: &SelectionViewParams) -> String {
    params
        .items
        .iter()
        .map(|item| {
            format!(
                "{} | {} | {} | {} | {}",
                item.name,
                item.description.as_deref().unwrap_or_default(),
                if item.is_disabled {
                    "disabled"
                } else {
                    "enabled"
                },
                if item.is_current { "current" } else { "-" },
                item.toggle
                    .as_ref()
                    .map(|toggle| if toggle.is_on { "on" } else { "off" })
                    .unwrap_or("-")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn rotation_editor_keeps_fixed_target_and_automatic_membership_independent() {
    let mut app = make_test_app().await;
    app.account_slots = vec![
        slot(1, "default", AccountSlotStatus::Ready, None),
        slot(2, "failed", AccountSlotStatus::Failed, None),
        slot(
            3,
            "new-busy",
            AccountSlotStatus::LoginRequired,
            Some("login-1"),
        ),
    ];
    app.account_rotation_available = true;
    app.account_runtime = Some((
        "epoch-a".to_string(),
        runtime(ThreadAccountRotationSnapshot {
            mode: ThreadAccountRotationMode::QuotaAware,
            fixed_account_slot_id: Some("failed".to_string()),
            automatic_account_slot_ids: vec!["default".to_string(), "failed".to_string()],
            revision: 4,
            last_committed_account_slot_id: Some("default".to_string()),
        }),
    ));

    let params = app.account_rotation_view_params().expect("rotation view");
    assert_snapshot!(rendered_items(&params), @r"
    Mode: Fixed | Use this mode starting with the next user turn | enabled | - | -
    Mode: Quota aware | Selected rotation mode | enabled | current | -
    Mode: Round robin | Use this mode starting with the next user turn | enabled | - | -
    Mode: Exhaust then next | Use this mode starting with the next user turn | enabled | - | -
    Fixed: 1. default | Ready · Current runtime | enabled | - | -
    Fixed: 2. failed | Login failed · Error: refreshUnavailable | enabled | current | -
    Fixed: 3. new-busy | Login required · Login in progress | enabled | - | -
    Automatic: 1. default | Ready | enabled | - | on
    Automatic: 2. failed | Login failed · Error: refreshUnavailable | enabled | - | on
    Automatic: 3. new-busy | Login required · Login in progress | enabled | - | off
    ");
}
