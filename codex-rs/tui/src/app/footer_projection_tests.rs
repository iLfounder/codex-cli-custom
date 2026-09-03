use super::project_footer_runtime;
use crate::app::account_validation::AccountSlotUpdateDisposition;
use crate::app::test_support::make_test_app;
use crate::app::test_support::test_session_runtime;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use codex_app_server_protocol::AccountSlotCapability;
use codex_app_server_protocol::AccountSlotHealth;
use codex_app_server_protocol::AccountSlotQuotaMeter;
use codex_app_server_protocol::AccountSlotQuotaSnapshot;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::SessionRuntimeAccountRef;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use codex_app_server_protocol::ThreadAccountRotationMode;
use codex_app_server_protocol::ThreadAccountRotationReadResponse;
use codex_app_server_protocol::ThreadAccountRotationSnapshot;
use codex_app_server_protocol::ThreadAccountRotationSource;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

const THREAD_ID: &str = "00000000-0000-0000-0000-000000000001";

fn rotation(revision: u64) -> ThreadAccountRotationSnapshot {
    ThreadAccountRotationSnapshot {
        mode: ThreadAccountRotationMode::QuotaAware,
        fixed_account_slot_id: None,
        automatic_account_slot_ids: vec!["C1".to_string(), "C2".to_string()],
        revision,
        last_committed_account_slot_id: Some("C1".to_string()),
        source: ThreadAccountRotationSource::Global,
        global_profile_revision: Some(revision),
    }
}

fn runtime(revision: u64, slot_id: &str) -> SessionRuntimeSnapshot {
    let mut runtime = test_session_runtime(THREAD_ID, revision);
    runtime.account.current = Some(SessionRuntimeAccountRef {
        account_slot_id: slot_id.to_string(),
        execution_generation: revision,
    });
    runtime.account.rotation = Some(rotation(revision));
    runtime
}

fn slot(number: u32, revision: u64) -> AccountSlotSnapshot {
    AccountSlotSnapshot {
        account_slot_id: format!("C{number}"),
        account_number: number,
        label: format!("private-account-label-{number}"),
        is_default: number == 1,
        status: AccountSlotStatus::Ready,
        health: AccountSlotHealth::Healthy,
        quota: Some(AccountSlotQuotaSnapshot {
            meters: vec![AccountSlotQuotaMeter {
                id: "weekly".to_string(),
                label: Some("Week".to_string()),
                remaining_percent: 70 - number,
                resets_at: None,
            }],
            observed_at: 0,
            stale_at: 1,
        }),
        auth_mode: None,
        attempt_generation: 1,
        registry_revision: revision,
        active_login_operation_id: None,
        error_code: None,
        actions: Vec::new(),
        updated_at: 0,
    }
}

fn capability() -> AccountSlotCapability {
    AccountSlotCapability {
        available: false,
        deny_reason: Some("not display-safe".to_string()),
    }
}

#[tokio::test]
async fn accepted_c2_state_is_unchanged_by_stale_c1_events() {
    let mut app = make_test_app().await;
    app.active_thread_id = Some(ThreadId::from_string(THREAD_ID).expect("valid thread id"));
    app.account_inventory_epoch = Some("epoch-a".to_string());
    app.account_registry_revision = 1;
    app.account_slot_capability = Some(capability());
    app.account_slots = vec![slot(1, 1)];
    app.account_rotation_available = true;
    assert!(app.handle_account_runtime_changed("epoch-a".to_string(), runtime(1, "C1")));

    assert_eq!(
        app.handle_account_slot_changed(2, slot(2, 2)),
        AccountSlotUpdateDisposition::Successor
    );
    assert!(app.handle_account_runtime_changed("epoch-a".to_string(), runtime(2, "C2")));
    let accepted = project_footer_runtime(
        Some(THREAD_ID),
        Some("Current thread"),
        app.account_runtime.as_ref().map(|(_, runtime)| runtime),
        app.account_slot_capability.as_ref(),
        &app.account_slots,
    );
    assert_eq!(
        (
            accepted.managed_slot_label.as_deref(),
            accepted.managed_slot_id.as_deref(),
            accepted.managed_slot_health.as_deref(),
            accepted.managed_slot_quota.as_deref(),
            accepted.thread_name.as_deref(),
        ),
        (
            Some("2"),
            Some("C2"),
            Some("healthy"),
            Some("Week 68%"),
            Some("Current thread"),
        )
    );
    let mismatched = project_footer_runtime(
        Some(THREAD_ID),
        Some("Current thread"),
        Some(&runtime(2, "C3")),
        app.account_slot_capability.as_ref(),
        &app.account_slots,
    );
    assert_eq!(
        (
            mismatched.managed_slot_label,
            mismatched.managed_slot_id,
            mismatched.managed_slot_health,
            mismatched.managed_slot_quota,
        ),
        (None, None, None, None)
    );
    assert!(
        project_footer_runtime(
            Some(THREAD_ID),
            Some("Current thread"),
            app.account_runtime.as_ref().map(|(_, runtime)| runtime),
            None,
            &app.account_slots,
        )
        .managed_slot_id
        .is_none()
    );

    assert!(!app.handle_account_runtime_changed("epoch-a".to_string(), runtime(1, "C1")));
    assert_eq!(
        app.handle_account_slot_changed(1, slot(1, 1)),
        AccountSlotUpdateDisposition::Stale
    );
    app.account_rotation_request_generation = 7;
    app.handle_account_rotation_loaded(
        ThreadId::from_string(THREAD_ID).expect("valid thread id"),
        "epoch-old".to_string(),
        7,
        Ok(ThreadAccountRotationReadResponse {
            rotation: rotation(99),
        }),
    );
    app.handle_account_rotation_loaded(
        ThreadId::from_string(THREAD_ID).expect("valid thread id"),
        "epoch-old".to_string(),
        7,
        Err("delayed failure".to_string()),
    );
    let after_stale = project_footer_runtime(
        Some(THREAD_ID),
        Some("Current thread"),
        app.account_runtime.as_ref().map(|(_, runtime)| runtime),
        app.account_slot_capability.as_ref(),
        &app.account_slots,
    );

    assert_eq!(accepted, after_stale);
    assert_eq!(after_stale.managed_slot_id.as_deref(), Some("C2"));
}

#[test]
fn cached_thread_identity_projects_without_runtime_and_ignores_runtime_name() {
    let mut stale_runtime = runtime(3, "C1");
    stale_runtime.identity.name = Some("Stale runtime name".to_string());

    let without_runtime =
        project_footer_runtime(Some(THREAD_ID), Some("Cached rename"), None, None, &[]);
    let with_runtime = project_footer_runtime(
        Some(THREAD_ID),
        Some("Cached rename"),
        Some(&stale_runtime),
        None,
        &[],
    );

    assert_eq!(without_runtime.thread_id.as_deref(), Some(THREAD_ID));
    assert_eq!(
        without_runtime.thread_name.as_deref(),
        Some("Cached rename")
    );
    assert_eq!(with_runtime.thread_name.as_deref(), Some("Cached rename"));
}

#[tokio::test]
async fn widget_replacement_retains_only_the_exact_displayed_thread_runtime() {
    let mut app = make_test_app().await;
    let thread_a = ThreadId::from_string(THREAD_ID).expect("valid thread id");
    let thread_b = ThreadId::new();
    app.active_thread_id = Some(thread_a);
    app.account_inventory_epoch = Some("epoch-a".to_string());
    app.account_registry_revision = 100;
    app.account_runtime = Some(("epoch-a".to_string(), runtime(100, "C1")));
    let initial_request_generation = app.account_request_generation;

    let (same_thread_widget, _, _, _) = make_chatwidget_manual_with_sender().await;
    app.replace_chat_widget_for_thread(same_thread_widget, thread_a);
    assert_eq!(
        app.account_runtime
            .as_ref()
            .map(|(_, runtime)| runtime.state_revision),
        Some(100)
    );
    assert!(app.account_request_generation > initial_request_generation);

    let (other_thread_widget, _, _, _) = make_chatwidget_manual_with_sender().await;
    app.replace_chat_widget_for_thread(other_thread_widget, thread_b);
    assert!(app.account_runtime.is_none());
    assert_eq!(app.account_inventory_epoch.as_deref(), Some("epoch-a"));
    assert_eq!(app.account_registry_revision, 100);

    app.active_thread_id = Some(thread_b);
    let mut thread_b_runtime = runtime(1, "C2");
    thread_b_runtime.thread_id = thread_b.to_string();
    assert!(app.handle_account_runtime_changed("epoch-a".to_string(), thread_b_runtime));
}
