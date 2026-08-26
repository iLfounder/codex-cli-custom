use super::edited_automatic_membership;
use codex_app_server_protocol::AccountSlotHealth;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
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
