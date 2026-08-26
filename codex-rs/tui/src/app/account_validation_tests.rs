use super::AccountSlotUpdateDisposition;
use super::revision_meets_lower_bound;
use super::runtime_revision_meets_lower_bound;
use crate::app::test_support::make_test_app;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use pretty_assertions::assert_eq;
#[test]
fn authoritative_revision_may_match_or_advance_but_not_regress() {
    assert_eq!(
        (
            revision_meets_lower_bound(10, 10),
            revision_meets_lower_bound(12, 10),
            revision_meets_lower_bound(9, 10),
        ),
        (true, true, false)
    );
}

#[test]
fn runtime_revision_may_restart_with_a_new_epoch_but_not_regress_within_an_epoch() {
    assert_eq!(
        (
            runtime_revision_meets_lower_bound(Some(("epoch-a", 10)), ("epoch-a", 10)),
            runtime_revision_meets_lower_bound(Some(("epoch-a", 10)), ("epoch-a", 12)),
            runtime_revision_meets_lower_bound(Some(("epoch-a", 10)), ("epoch-a", 9)),
            runtime_revision_meets_lower_bound(Some(("epoch-a", 10)), ("epoch-b", 1)),
        ),
        (true, true, false, true)
    );
}
fn snapshot(revision: u64, label: &str) -> AccountSlotSnapshot {
    AccountSlotSnapshot {
        account_slot_id: "secondary".to_string(),
        account_number: 2,
        label: label.to_string(),
        is_default: false,
        status: AccountSlotStatus::Ready,
        auth_mode: None,
        attempt_generation: 1,
        registry_revision: revision,
        active_login_operation_id: None,
        error_code: None,
        actions: Vec::new(),
        updated_at: 0,
    }
}
#[tokio::test]
async fn slot_updates_apply_only_exact_successors_without_reopening_closed_ui() {
    let mut app = make_test_app().await;
    app.account_registry_revision = 5;
    app.account_slots = vec![snapshot(5, "original")];
    let stale = app.handle_account_slot_changed(5, snapshot(5, "stale"));
    let successor = app.handle_account_slot_changed(6, snapshot(6, "successor"));
    let gap = app.handle_account_slot_changed(8, snapshot(8, "gap"));
    assert_eq!(
        (
            stale,
            successor,
            gap,
            app.account_registry_revision,
            app.account_slots[0].label.as_str(),
            app.chat_widget.no_modal_or_popup_active(),
        ),
        (
            AccountSlotUpdateDisposition::Stale,
            AccountSlotUpdateDisposition::Successor,
            AccountSlotUpdateDisposition::Gap,
            6,
            "successor",
            true,
        )
    );
}
