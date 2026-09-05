use super::edited_automatic_membership;
use crate::app::test_support::make_test_app;
use crate::app::test_support::test_session_runtime;
use codex_app_server_protocol::AccountSlotHealth;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::ThreadAccountRotationMode;
use codex_app_server_protocol::ThreadAccountRotationSnapshot;
use codex_app_server_protocol::ThreadAccountRotationSource;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;

fn global_slot(number: u32) -> AccountSlotSnapshot {
    AccountSlotSnapshot {
        account_slot_id: format!("C{number}"),
        account_number: number,
        label: format!("Account {number}"),
        is_default: number == 1,
        status: AccountSlotStatus::Ready,
        health: AccountSlotHealth::Healthy,
        quota: None,
        auth_mode: None,
        attempt_generation: 1,
        registry_revision: 1,
        active_login_operation_id: None,
        error_code: None,
        actions: Vec::new(),
        updated_at: 0,
    }
}

#[test]
fn automatic_membership_edit_converges_to_the_visible_global_account_set() {
    let visible = vec![global_slot(1), global_slot(2), global_slot(3)];
    let existing = vec!["C1".to_string(), "default".to_string()];

    assert_eq!(
        edited_automatic_membership(&visible, existing.clone(), "C2".to_string(), true),
        vec!["C1", "C2"]
    );
    assert_eq!(
        edited_automatic_membership(&visible, existing, "C1".to_string(), false),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn rotation_responses_require_the_current_epoch_and_preserve_monotonic_revision() {
    let mut app = make_test_app().await;
    let thread_id = ThreadId::new();
    app.active_thread_id = Some(thread_id);
    app.account_rotation_available = true;
    let mut runtime = test_session_runtime(&thread_id.to_string(), 5);
    runtime.account.rotation = Some(ThreadAccountRotationSnapshot {
        mode: ThreadAccountRotationMode::Fixed,
        fixed_account_slot_id: Some("C1".to_string()),
        automatic_account_slot_ids: Vec::new(),
        revision: 5,
        last_committed_account_slot_id: Some("C1".to_string()),
        source: ThreadAccountRotationSource::Override,
        global_profile_revision: Some(5),
    });
    app.account_runtime = Some(("epoch-new".to_string(), runtime));

    assert!(!app.account_rotation_response_is_current(thread_id, "epoch-old"));
    assert!(app.account_rotation_response_is_current(thread_id, "epoch-new"));
    let mut response = app.account_rotation_snapshot().expect("rotation").clone();
    response.revision = 4;
    assert!(!app.apply_account_rotation(thread_id, response.clone()));
    assert_eq!(
        app.account_rotation_snapshot().map(|state| state.revision),
        Some(5)
    );
    response.revision = 6;
    assert!(app.apply_account_rotation(thread_id, response));
    assert_eq!(
        app.account_rotation_snapshot().map(|state| state.revision),
        Some(6)
    );
}
