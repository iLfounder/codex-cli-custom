use super::*;
use crate::app::test_support::make_test_app;
use codex_app_server_protocol::AccountSlotHealth;
use codex_app_server_protocol::AccountSlotQuotaMeter;
use codex_app_server_protocol::AccountSlotQuotaSnapshot;
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
use pretty_assertions::assert_eq;

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
        health: match status {
            AccountSlotStatus::Ready => AccountSlotHealth::Healthy,
            AccountSlotStatus::Failed => AccountSlotHealth::Degraded,
            AccountSlotStatus::LoginRequired => AccountSlotHealth::Unavailable,
        },
        quota: (status == AccountSlotStatus::Ready).then(|| AccountSlotQuotaSnapshot {
            meters: vec![AccountSlotQuotaMeter {
                id: "weekly".to_string(),
                label: Some("Weekly".to_string()),
                remaining_percent: 72,
                resets_at: Some(1_800_000_000),
            }],
            observed_at: 1_700_000_000,
            stale_at: 1_700_000_300,
        }),
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
                account_slot_id: "C1".to_string(),
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
        slot(1, "C1", AccountSlotStatus::Ready, None),
        slot(2, "C2", AccountSlotStatus::Failed, None),
        slot(3, "C3", AccountSlotStatus::LoginRequired, Some("login-1")),
    ];
    app.account_rotation_available = true;
    app.account_runtime = Some((
        "epoch-a".to_string(),
        runtime(ThreadAccountRotationSnapshot {
            mode: ThreadAccountRotationMode::QuotaAware,
            fixed_account_slot_id: Some("C2".to_string()),
            automatic_account_slot_ids: vec!["C1".to_string(), "C2".to_string()],
            revision: 4,
            last_committed_account_slot_id: Some("C1".to_string()),
        }),
    ));

    let params = app.account_rotation_view_params().expect("rotation view");
    assert_eq!(
        app.account_rotation_summary().as_deref(),
        Some("Desired: Quota aware · Actual: C1 · 2 automatic accounts")
    );
    assert_eq!(
        params.subtitle.as_deref(),
        Some("Desired revision 4 · actual execution changes only when a turn starts")
    );
    assert_snapshot!(rendered_items(&params), @r"
    Mode: Fixed | Use this mode starting with the next user turn | enabled | - | -
    Mode: Quota aware | Selected rotation mode | enabled | current | -
    Mode: Round robin | Use this mode starting with the next user turn | enabled | - | -
    Mode: Exhaust then next | Use this mode starting with the next user turn | enabled | - | -
    Fixed: 1. C1 | Ready · Projection healthy · Weekly 72% left, resets at 1800000000 · quota fresh until 1700000300 · Current runtime | enabled | - | -
    Fixed: 2. C2 | Login failed · Error: refreshUnavailable · Projection stale | enabled | current | -
    Fixed: 3. C3 | Login required · Login in progress · Projection unavailable | enabled | - | -
    Automatic: 1. C1 | Ready · Projection healthy · Weekly 72% left, resets at 1800000000 · quota fresh until 1700000300 | enabled | - | on
    Automatic: 2. C2 | Login failed · Error: refreshUnavailable · Projection stale | enabled | - | on
    Automatic: 3. C3 | Login required · Login in progress · Projection unavailable | disabled | - | off
    ");
}
