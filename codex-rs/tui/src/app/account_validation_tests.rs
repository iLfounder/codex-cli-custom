use super::AccountSlotUpdateDisposition;
use super::RuntimeRevisionIdentity;
use super::revision_meets_lower_bound;
use super::runtime_revision_meets_lower_bound;
use crate::app::test_support::make_test_app;
use codex_app_server_protocol::AccountSlotCapability;
use codex_app_server_protocol::AccountSlotCatalogKind;
use codex_app_server_protocol::AccountSlotHealth;
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
    let identity = |instance_epoch, thread_id, state_revision| RuntimeRevisionIdentity {
        instance_epoch,
        thread_id,
        state_revision,
    };
    assert_eq!(
        (
            runtime_revision_meets_lower_bound(
                Some(identity("epoch-a", "thread-a", 10)),
                identity("epoch-a", "thread-a", 10),
            ),
            runtime_revision_meets_lower_bound(
                Some(identity("epoch-a", "thread-a", 10)),
                identity("epoch-a", "thread-a", 12),
            ),
            runtime_revision_meets_lower_bound(
                Some(identity("epoch-a", "thread-a", 10)),
                identity("epoch-a", "thread-a", 9),
            ),
            runtime_revision_meets_lower_bound(
                Some(identity("epoch-a", "thread-a", 10)),
                identity("epoch-a", "thread-b", 1),
            ),
            runtime_revision_meets_lower_bound(
                Some(identity("epoch-a", "thread-a", 10)),
                identity("epoch-b", "thread-a", 1),
            ),
        ),
        (true, true, false, true, true)
    );
}

fn authoritative_inventory(app: &mut crate::app::App, revision: u64) {
    app.account_inventory_epoch = Some("epoch-a".to_string());
    app.account_registry_revision = revision;
    app.account_slot_capability = Some(AccountSlotCapability {
        available: true,
        deny_reason: None,
    });
}
fn snapshot(revision: u64, label: &str) -> AccountSlotSnapshot {
    AccountSlotSnapshot {
        account_slot_id: "C2".to_string(),
        account_number: 2,
        label: label.to_string(),
        is_default: false,
        status: AccountSlotStatus::Ready,
        health: AccountSlotHealth::Healthy,
        quota: None,
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
async fn legacy_slot_update_cannot_advance_global_revision_or_block_a_canonical_successor() {
    let mut app = make_test_app().await;
    app.account_catalog_kind = Some(AccountSlotCatalogKind::Global);
    authoritative_inventory(&mut app, 5);
    app.account_slots = vec![snapshot(5, "global")];
    let mut legacy = snapshot(99, "legacy");
    legacy.account_slot_id = "default".to_string();
    let legacy_disposition = app.handle_account_slot_changed(99, legacy);
    let successor_disposition = app.handle_account_slot_changed(6, snapshot(6, "successor"));

    assert_eq!(
        (
            legacy_disposition,
            successor_disposition,
            app.account_registry_revision,
            app.account_slots
                .iter()
                .map(|slot| (slot.account_slot_id.as_str(), slot.label.as_str()))
                .collect::<Vec<_>>(),
        ),
        (
            AccountSlotUpdateDisposition::Stale,
            AccountSlotUpdateDisposition::Successor,
            6,
            vec![("C2", "successor")],
        )
    );
}

#[tokio::test]
async fn legacy_catalog_accepts_legacy_slot_successors() {
    let mut app = make_test_app().await;
    app.account_catalog_kind = Some(AccountSlotCatalogKind::Legacy);
    authoritative_inventory(&mut app, 5);
    let mut original = snapshot(5, "original");
    original.account_slot_id = "default".to_string();
    app.account_slots = vec![original];
    let mut successor = snapshot(6, "successor");
    successor.account_slot_id = "default".to_string();

    assert_eq!(
        (
            app.handle_account_slot_changed(6, successor),
            app.account_registry_revision,
            app.account_slots[0].label.as_str(),
        ),
        (AccountSlotUpdateDisposition::Successor, 6, "successor")
    );
}
#[tokio::test]
async fn slot_updates_apply_only_exact_successors_without_reopening_closed_ui() {
    let mut app = make_test_app().await;
    authoritative_inventory(&mut app, 5);
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
            app.account_slots.is_empty(),
            app.account_slot_capability.is_none(),
            app.chat_widget.no_modal_or_popup_active(),
        ),
        (
            AccountSlotUpdateDisposition::Stale,
            AccountSlotUpdateDisposition::Successor,
            AccountSlotUpdateDisposition::Gap,
            8,
            true,
            true,
            true,
        )
    );
}

#[tokio::test]
async fn invalid_inventory_cannot_be_rebuilt_by_point_updates() {
    let mut app = make_test_app().await;
    authoritative_inventory(&mut app, 5);
    app.account_slots = vec![snapshot(5, "original")];

    assert_eq!(
        app.handle_account_slot_changed(7, snapshot(7, "gap")),
        AccountSlotUpdateDisposition::Gap
    );
    assert_eq!(
        app.handle_account_slot_changed(8, snapshot(8, "point-after-gap")),
        AccountSlotUpdateDisposition::Gap
    );
    assert_eq!(
        (
            app.account_registry_revision,
            app.account_slots.is_empty(),
            app.account_slot_capability.is_none(),
        ),
        (8, true, true)
    );

    assert!(app.handle_account_slot_inventory_changed(10));
    assert!(!app.handle_account_slot_inventory_changed(10));
    assert_eq!(app.account_registry_revision, 10);
    assert!(app.account_slots.is_empty());
    assert!(app.account_slot_capability.is_none());
}
