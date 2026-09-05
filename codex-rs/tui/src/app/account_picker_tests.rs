use super::*;
use crate::app::test_support::make_test_app;
use codex_app_server_protocol::AccountSlotCapability;
use codex_app_server_protocol::AccountSlotCatalogKind;
use codex_app_server_protocol::AccountSlotHealth;
use codex_app_server_protocol::AccountSlotSnapshot;
use codex_app_server_protocol::AccountSlotStatus;
use codex_app_server_protocol::SESSION_RUNTIME_ACCOUNT_ROTATION_CAPABILITY;
use codex_app_server_protocol::SessionRuntimeAccountBinding;
use codex_app_server_protocol::SessionRuntimeAccountRef;
use codex_app_server_protocol::SessionRuntimeAccountSwitchState;
use codex_app_server_protocol::SessionRuntimeCapability;
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
use codex_app_server_protocol::ThreadAccountRotationMode;
use codex_app_server_protocol::ThreadAccountRotationSnapshot;
use codex_app_server_protocol::ThreadAccountRotationSource;
use pretty_assertions::assert_eq;

const THREAD_ID: &str = "00000000-0000-0000-0000-000000000001";

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

fn display_snapshot_thread(app: &mut App) {
    app.active_thread_id = Some(ThreadId::from_string(THREAD_ID).expect("valid thread id"));
}

fn picker_snapshot(
    epoch: &str,
    thread_id: &str,
    registry_revision: u64,
    state_revision: u64,
) -> AccountPickerSnapshot {
    AccountPickerSnapshot {
        slots: AccountSlotsSnapshot {
            data: Vec::new(),
            registry_revision,
            catalog_kind: AccountSlotCatalogKind::Global,
            multi_account: AccountSlotCapability {
                available: true,
                deny_reason: None,
            },
        },
        runtime: ThreadRuntimeSnapshot {
            instance_epoch: epoch.to_string(),
            snapshot: runtime_snapshot(thread_id, state_revision),
            capabilities: Vec::new(),
        },
    }
}

fn listed_account(account_slot_id: &str, account_number: u32) -> AccountSlotSnapshot {
    AccountSlotSnapshot {
        account_slot_id: account_slot_id.to_string(),
        account_number,
        label: account_slot_id.to_string(),
        is_default: account_number == 1,
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

#[tokio::test]
async fn account_snapshot_preserves_the_authoritative_catalog_domain() {
    let mut app = make_test_app().await;
    display_snapshot_thread(&mut app);
    let mut snapshot = picker_snapshot("epoch-a", THREAD_ID, 1, 1);
    snapshot.slots.data = vec![
        listed_account("default", 1),
        listed_account("C1", 1),
        listed_account("C2", 2),
        listed_account("slot-uuid", 3),
    ];

    assert_eq!(app.apply_account_snapshot(snapshot), true);
    assert_eq!(
        app.account_slots
            .iter()
            .map(|slot| slot.account_slot_id.as_str())
            .collect::<Vec<_>>(),
        vec!["default", "C1", "C2", "slot-uuid"]
    );
}

#[tokio::test]
async fn authoritative_catalog_domain_change_resets_the_revision_comparison() {
    let mut app = make_test_app().await;
    display_snapshot_thread(&mut app);
    app.account_catalog_kind = Some(AccountSlotCatalogKind::Legacy);
    app.account_registry_revision = 50;
    app.account_inventory_epoch = Some("epoch-a".to_string());
    app.account_runtime = Some(("epoch-a".to_string(), runtime_snapshot(THREAD_ID, 1)));
    let mut global = picker_snapshot("epoch-a", THREAD_ID, 1, 1);
    global.slots.data = vec![listed_account("C1", 1)];

    assert_eq!(app.apply_account_snapshot(global), true);
    assert_eq!(
        (app.account_catalog_kind, app.account_registry_revision),
        (Some(AccountSlotCatalogKind::Global), 1)
    );

    let mut legacy = picker_snapshot("epoch-a", THREAD_ID, 70, 1);
    legacy.slots.catalog_kind = AccountSlotCatalogKind::Legacy;
    legacy.slots.registry_revision = 0;
    legacy.slots.data = vec![listed_account("default", 1)];
    assert_eq!(app.apply_account_snapshot(legacy), true);
    assert_eq!(
        (app.account_catalog_kind, app.account_registry_revision),
        (Some(AccountSlotCatalogKind::Legacy), 0)
    );
}

#[tokio::test]
async fn new_runtime_epoch_accepts_authoritative_revision_reset() {
    let mut app = make_test_app().await;
    display_snapshot_thread(&mut app);
    app.account_registry_revision = 20;
    app.account_inventory_epoch = Some("epoch-a".to_string());
    app.account_runtime = Some(("epoch-a".to_string(), runtime_snapshot(THREAD_ID, 20)));

    assert_eq!(
        app.apply_account_snapshot(picker_snapshot("epoch-b", THREAD_ID, 0, 1)),
        true
    );
    assert_eq!(
        (
            app.account_registry_revision,
            app.account_runtime
                .as_ref()
                .map(|(epoch, runtime)| (epoch.as_str(), runtime.state_revision)),
        ),
        (0, Some(("epoch-b", 1)))
    );
    assert_eq!(app.account_inventory_epoch.as_deref(), Some("epoch-b"));
}

#[tokio::test]
async fn same_runtime_epoch_rejects_revision_regression() {
    let mut app = make_test_app().await;
    display_snapshot_thread(&mut app);
    app.account_registry_revision = 20;
    app.account_inventory_epoch = Some("epoch-a".to_string());
    app.account_runtime = Some(("epoch-a".to_string(), runtime_snapshot(THREAD_ID, 20)));

    assert_eq!(
        app.apply_account_snapshot(picker_snapshot("epoch-a", THREAD_ID, 19, 19)),
        false
    );
    assert_eq!(app.account_registry_revision, 20);

    app.account_slots.clear();
    app.account_slot_capability = None;
    assert!(app.apply_account_snapshot(picker_snapshot("epoch-a", THREAD_ID, 20, 20,)));
    assert!(app.account_slot_capability.is_some());
}

#[tokio::test]
async fn same_runtime_epoch_merges_each_fresh_projection_independently() {
    let mut app = make_test_app().await;
    display_snapshot_thread(&mut app);
    app.account_registry_revision = 20;
    app.account_inventory_epoch = Some("epoch-a".to_string());
    app.account_runtime = Some(("epoch-a".to_string(), runtime_snapshot(THREAD_ID, 20)));

    assert_eq!(
        app.apply_account_snapshot(picker_snapshot("epoch-a", THREAD_ID, 19, 21)),
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
async fn fresh_slots_do_not_accept_stale_runtime_capability_or_rotation() {
    let mut app = make_test_app().await;
    display_snapshot_thread(&mut app);
    let accepted_rotation = ThreadAccountRotationSnapshot {
        mode: ThreadAccountRotationMode::Fixed,
        fixed_account_slot_id: Some("C1".to_string()),
        automatic_account_slot_ids: Vec::new(),
        revision: 5,
        last_committed_account_slot_id: Some("C1".to_string()),
        source: ThreadAccountRotationSource::Override,
        global_profile_revision: Some(5),
    };
    let mut accepted_runtime = runtime_snapshot(THREAD_ID, 20);
    accepted_runtime.account.current = Some(SessionRuntimeAccountRef {
        account_slot_id: "C1".to_string(),
        execution_generation: 1,
    });
    accepted_runtime.account.rotation = Some(accepted_rotation.clone());
    app.account_inventory_epoch = Some("epoch-a".to_string());
    app.account_registry_revision = 20;
    app.account_slot_capability = Some(AccountSlotCapability {
        available: true,
        deny_reason: None,
    });
    app.account_rotation_available = true;
    app.account_runtime = Some(("epoch-a".to_string(), accepted_runtime));

    let mut partial = picker_snapshot("epoch-a", THREAD_ID, 21, 19);
    partial.slots.data = vec![listed_account("C1", 1)];
    partial.runtime.snapshot.account.rotation = Some(ThreadAccountRotationSnapshot {
        mode: ThreadAccountRotationMode::RoundRobin,
        revision: 99,
        ..accepted_rotation.clone()
    });
    partial.runtime.capabilities = Vec::new();

    assert!(!app.apply_account_snapshot(partial));
    assert_eq!(app.account_registry_revision, 21);
    assert_eq!(app.account_slots.len(), 1);
    assert!(app.account_rotation_available);
    assert_eq!(app.account_rotation_snapshot(), Some(&accepted_rotation));
    assert_eq!(
        crate::app::footer_projection::project_footer_runtime(
            Some(THREAD_ID),
            /*displayed_thread_name*/ None,
            app.account_runtime.as_ref().map(|(_, runtime)| runtime),
            app.account_slot_capability.as_ref(),
            &app.account_slots,
        )
        .managed_slot_id
        .as_deref(),
        Some("C1")
    );
}

#[tokio::test]
async fn same_epoch_thread_change_preserves_inventory_and_rejects_foreign_slots() {
    const THREAD_B: &str = "00000000-0000-0000-0000-000000000002";
    let mut app = make_test_app().await;
    display_snapshot_thread(&mut app);
    app.account_inventory_epoch = Some("epoch-a".to_string());
    app.account_registry_revision = 100;
    app.account_slot_capability = Some(AccountSlotCapability {
        available: true,
        deny_reason: None,
    });
    app.account_runtime = Some(("epoch-a".to_string(), runtime_snapshot(THREAD_ID, 100)));

    app.active_thread_id = Some(ThreadId::from_string(THREAD_B).expect("valid thread id"));
    assert!(!app.apply_account_snapshot(picker_snapshot("epoch-a", THREAD_B, 1, 1,)));
    assert_eq!(app.account_inventory_epoch.as_deref(), Some("epoch-a"));
    assert_eq!(app.account_registry_revision, 100);
    assert_eq!(
        app.account_runtime.as_ref().map(|(epoch, runtime)| (
            epoch.as_str(),
            runtime.thread_id.as_str(),
            runtime.state_revision,
        )),
        Some(("epoch-a", THREAD_B, 1))
    );

    let rejected = picker_snapshot("epoch-c", THREAD_ID, 50, 2);
    assert!(!app.apply_account_snapshot(rejected));
    assert_eq!(app.account_inventory_epoch.as_deref(), Some("epoch-a"));
    assert_eq!(app.account_registry_revision, 100);
}

#[tokio::test]
async fn rotation_is_exposed_only_with_the_matching_available_capability() {
    let rotation = ThreadAccountRotationSnapshot {
        mode: ThreadAccountRotationMode::Fixed,
        fixed_account_slot_id: None,
        automatic_account_slot_ids: Vec::new(),
        revision: 1,
        last_committed_account_slot_id: None,
        source: ThreadAccountRotationSource::LegacyFixed,
        global_profile_revision: None,
    };
    let mut app = make_test_app().await;
    display_snapshot_thread(&mut app);
    let mut hidden = picker_snapshot("epoch-a", THREAD_ID, 1, 1);
    hidden.runtime.snapshot.account.rotation = Some(rotation.clone());
    assert_eq!(app.apply_account_snapshot(hidden), true);
    assert_eq!(app.account_rotation_snapshot(), None);

    let mut visible = picker_snapshot("epoch-b", THREAD_ID, 1, 1);
    visible.runtime.snapshot.account.rotation = Some(rotation.clone());
    visible.runtime.capabilities = vec![SessionRuntimeCapability {
        name: SESSION_RUNTIME_ACCOUNT_ROTATION_CAPABILITY.to_string(),
        available: true,
        deny_reason: None,
    }];
    assert_eq!(app.apply_account_snapshot(visible), true);
    assert_eq!(app.account_rotation_snapshot(), Some(&rotation));
}

enum SwitchReadyTiming {
    BeforeEscape,
    AfterEscape,
}

async fn account_switch_validation_preserves_picker_dismissal(timing: SwitchReadyTiming) {
    let mut app = make_test_app().await;
    let app_server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config))
        .await
        .expect("test app server");
    display_snapshot_thread(&mut app);
    let thread_id = ThreadId::from_string(THREAD_ID).expect("thread id");
    let mut initial = picker_snapshot("epoch-a", THREAD_ID, 1, 1);
    initial.slots.data = vec![listed_account("C1", 1), listed_account("C2", 2)];
    initial.runtime.snapshot.account.current = Some(SessionRuntimeAccountRef {
        account_slot_id: "C1".to_string(),
        execution_generation: 1,
    });
    assert!(app.apply_account_snapshot(initial));
    // Account detail acceptance leaves this parent list open.
    app.open_account_picker(&app_server);
    assert!(app.replace_account_picker_if_present(Some("C2")));
    app.pending_account_control = Some(PendingAccountControl::Switch {
        operation_id: "switch-c2".to_string(),
        thread_id,
        target_slot_id: "C2".to_string(),
        instance_epoch: "epoch-a".to_string(),
        ready_state_revision: None,
        ready_generation: None,
        validation_in_flight: false,
    });
    if matches!(timing, SwitchReadyTiming::AfterEscape) {
        app.chat_widget
            .handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.chat_widget.no_modal_or_popup_active());
    }
    // Exercise the real switch-reply -> Ready -> validation-request path.
    app.handle_account_switch_finished(
        &app_server,
        "switch-c2",
        Ok(SessionRuntimeOperation {
            operation_id: "switch-c2".to_string(),
            request_fingerprint: "synthetic-switch".to_string(),
            action: SessionRuntimeOperationAction::ThreadAccountSwitch,
            status: SessionRuntimeOperationStatus::Ready,
            thread_id: Some(THREAD_ID.to_string()),
            account_slot_id: Some("C2".to_string()),
            state_revision: Some(2),
            writer_generation: None,
            execution_generation: Some(2),
            error: None,
            updated_at: 0,
        }),
    );
    assert!(matches!(
        app.pending_account_control,
        Some(PendingAccountControl::Switch {
            ready_state_revision: Some(2),
            ready_generation: Some(2),
            validation_in_flight: true,
            ..
        })
    ));
    if matches!(timing, SwitchReadyTiming::BeforeEscape) {
        app.chat_widget
            .handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    }
    assert!(app.chat_widget.no_modal_or_popup_active());

    // Deliver the authoritative reply after dismissal, without depending on
    // transport scheduling or requiring a real account switch in this UI test.
    let mut refreshed = picker_snapshot("epoch-a", THREAD_ID, 2, 2);
    refreshed.slots.data = vec![listed_account("C1", 1), listed_account("C2", 2)];
    for slot in &mut refreshed.slots.data {
        slot.registry_revision = 2;
    }
    refreshed.runtime.snapshot.account.current = Some(SessionRuntimeAccountRef {
        account_slot_id: "C2".to_string(),
        execution_generation: 2,
    });
    let expected = refreshed.runtime.snapshot.clone();
    app.handle_account_state_refreshed(
        &app_server,
        thread_id,
        app.account_request_generation,
        Ok(refreshed),
    );
    assert_eq!(app.account_runtime, Some(("epoch-a".to_string(), expected)));
    assert!(app.pending_account_control.is_none());
    assert!(app.chat_widget.no_modal_or_popup_active());
    app_server
        .shutdown()
        .await
        .expect("shutdown test app server");
}

#[tokio::test]
async fn switch_ready_then_escape_dismisses_the_only_account_picker() {
    account_switch_validation_preserves_picker_dismissal(SwitchReadyTiming::BeforeEscape).await;
}

#[tokio::test]
async fn switch_ready_after_escape_does_not_reopen_the_account_picker() {
    account_switch_validation_preserves_picker_dismissal(SwitchReadyTiming::AfterEscape).await;
}
