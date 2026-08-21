use super::*;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotActionAvailability;
use codex_app_server_protocol::AccountSlotLogoutParams;
use codex_core::config::ConfigBuilder;
use codex_login::CodexAuth;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

const SECOND_SLOT_ID: &str = "11111111111141118111111111111111";
const THIRD_SLOT_ID: &str = "22222222222242228222222222222222";

async fn registry_for_home(process_home: &Path) -> AccountRegistry {
    let config = Arc::new(
        ConfigBuilder::default()
            .codex_home(process_home.to_path_buf())
            .build()
            .await
            .expect("build config"),
    );
    let auth_manager = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::from_api_key("dummy"),
        process_home.to_path_buf(),
    );
    let models_manager = codex_core::build_models_manager(config.as_ref(), auth_manager.clone());
    AccountRegistry::new(config, auth_manager, models_manager)
}

fn manifest(process_home: &Path, revision: u64) -> AccountSlotsManifest {
    AccountSlotsManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        revision,
        slots: vec![
            AccountSlotManifest {
                account_slot_id: DEFAULT_SLOT_ID.to_string(),
                label: "Default account".to_string(),
                auth_home: process_home.to_path_buf(),
                is_default: true,
                status: ManifestSlotStatus::Ready,
                attempt_generation: 0,
                updated_at: 10,
                error_code: None,
            },
            AccountSlotManifest {
                account_slot_id: SECOND_SLOT_ID.to_string(),
                label: "Second account".to_string(),
                auth_home: process_home.join(PRIVATE_HOMES_DIR).join(SECOND_SLOT_ID),
                is_default: false,
                status: ManifestSlotStatus::LoginRequired,
                attempt_generation: 2,
                updated_at: 20,
                error_code: None,
            },
        ],
    }
}

#[test]
fn manifest_round_trips_through_atomic_replace() {
    let process_home = tempdir().expect("temp process home");
    let path = process_home.path().join(MANIFEST_FILE);
    let first = manifest(process_home.path(), 1);
    first.persist(&path).expect("persist first manifest");
    assert_eq!(
        AccountSlotsManifest::load(&path, process_home.path()).expect("load first manifest"),
        Some(first)
    );

    let second = manifest(process_home.path(), 2);
    second.persist(&path).expect("replace manifest");
    assert_eq!(
        AccountSlotsManifest::load(&path, process_home.path()).expect("load second manifest"),
        Some(second)
    );
}

#[test]
fn manifest_rejects_non_private_secondary_home() {
    let process_home = tempdir().expect("temp process home");
    let outside_home = tempdir().expect("outside home");
    let mut manifest = manifest(process_home.path(), 1);
    manifest.slots[1].auth_home = outside_home.path().to_path_buf();

    let error = manifest
        .validate(process_home.path())
        .expect_err("outside account home must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn manifest_rejects_non_uuid_slot_and_lexical_path_escape() {
    let process_home = tempdir().expect("temp process home");
    let mut invalid_id = manifest(process_home.path(), 1);
    invalid_id.slots[1].account_slot_id = "..".to_string();
    invalid_id.slots[1].auth_home = process_home.path().join(PRIVATE_HOMES_DIR).join("..");

    let error = invalid_id
        .validate(process_home.path())
        .expect_err("non-UUID slot ID must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);

    let mut escaped_home = manifest(process_home.path(), 1);
    escaped_home.slots[1].auth_home = process_home
        .path()
        .join(PRIVATE_HOMES_DIR)
        .join(SECOND_SLOT_ID)
        .join("..")
        .join("..");
    let error = escaped_home
        .validate(process_home.path())
        .expect_err("lexical path escape must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[cfg(unix)]
#[test]
fn manifest_rejects_symlinked_private_home() {
    use std::os::unix::fs::symlink;

    let process_home = tempdir().expect("temp process home");
    let outside_home = tempdir().expect("outside home");
    let private_root = process_home.path().join(PRIVATE_HOMES_DIR);
    std::fs::create_dir(&private_root).expect("create private root");
    symlink(outside_home.path(), private_root.join(SECOND_SLOT_ID)).expect("create escaping link");

    let error = manifest(process_home.path(), 1)
        .validate(process_home.path())
        .expect_err("symlinked private home must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

#[test]
fn cursor_is_bound_to_registry_revision() {
    let encoded = encode_cursor(AccountSlotCursor {
        revision: 7,
        after_slot_id: SECOND_SLOT_ID.to_string(),
    })
    .expect("cursor should encode");
    let decoded = decode_cursor(&encoded, 7).expect("cursor should decode");
    assert_eq!(
        decoded,
        AccountSlotCursor {
            revision: 7,
            after_slot_id: SECOND_SLOT_ID.to_string(),
        }
    );
    assert!(matches!(
        decode_cursor(&encoded, 8),
        Err(CursorError::Stale)
    ));
}

#[tokio::test]
async fn slot_snapshot_releases_registry_guard_before_capability_lookup() {
    let process_home = tempdir().expect("temp process home");
    let registry = registry_for_home(process_home.path()).await;

    let snapshot = registry
        .slot_snapshot(DEFAULT_SLOT_ID)
        .await
        .expect("default snapshot");

    assert_eq!(snapshot.account_slot_id, DEFAULT_SLOT_ID);
    assert!(snapshot.is_default);
}

#[tokio::test]
async fn stale_completion_cannot_overwrite_finished_attempt() {
    let process_home = tempdir().expect("temp process home");
    let registry = registry_for_home(process_home.path()).await;
    let prepared = registry
        .prepare_slot_login(/*requested_slot_id*/ None, "operation-1".to_string())
        .await
        .expect("prepare slot");

    let failed = registry
        .finish_slot_login(
            &prepared,
            ManifestSlotStatus::Failed,
            Some("refreshUnavailable"),
        )
        .await
        .expect("finish failed attempt");
    let stale_ready = registry
        .finish_slot_login(&prepared, ManifestSlotStatus::Ready, None)
        .await
        .expect("stale completion");

    assert!(failed.is_some());
    assert_eq!(stale_ready, None);
    let snapshot = registry
        .slot_snapshot(&prepared.account_slot_id)
        .await
        .expect("slot snapshot");
    assert_eq!(snapshot.status, AccountSlotStatus::Failed);
    assert_eq!(snapshot.error_code.as_deref(), Some("refreshUnavailable"));
}

#[tokio::test]
async fn browser_login_owner_requires_exact_release() {
    let process_home = tempdir().expect("temp process home");
    let registry = registry_for_home(process_home.path()).await;
    let default = BrowserLoginOwner::Default("default-login".to_string());
    let slot = BrowserLoginOwner::Slot("slot-login".to_string());

    registry
        .try_begin_browser_login(default.clone())
        .await
        .expect("first owner");
    assert!(
        registry
            .try_begin_browser_login(slot.clone())
            .await
            .is_err()
    );
    registry.finish_browser_login(&slot).await;
    assert!(
        registry
            .try_begin_browser_login(slot.clone())
            .await
            .is_err()
    );
    registry.finish_browser_login(&default).await;
    registry
        .try_begin_browser_login(slot.clone())
        .await
        .expect("slot after exact release");
    registry.finish_browser_login(&slot).await;
}

#[tokio::test]
async fn reconcile_repairs_ready_slot_without_auth() {
    let process_home = tempdir().expect("temp process home");
    let mut slots = manifest(process_home.path(), 1);
    slots.slots[1].status = ManifestSlotStatus::Ready;
    slots
        .persist(&process_home.path().join(MANIFEST_FILE))
        .expect("persist manifest");
    let registry = registry_for_home(process_home.path()).await;

    registry.reconcile().await.expect("reconcile");

    let snapshot = registry
        .slot_snapshot(SECOND_SLOT_ID)
        .await
        .expect("slot snapshot");
    assert_eq!(snapshot.status, AccountSlotStatus::Failed);
    assert_eq!(snapshot.error_code.as_deref(), Some("authUnavailable"));
}

#[tokio::test]
async fn secondary_logout_revokes_exact_ready_slot_and_bumps_generation() {
    let process_home = tempdir().expect("temp process home");
    let mut slots = manifest(process_home.path(), 7);
    slots.slots[1].status = ManifestSlotStatus::Ready;
    slots
        .persist(&process_home.path().join(MANIFEST_FILE))
        .expect("persist manifest");
    let registry = registry_for_home(process_home.path()).await;
    let slot_auth = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::from_api_key("slot-key"),
        slots.slots[1].auth_home.clone(),
    );
    let slot_models = codex_core::build_models_manager(registry.config.as_ref(), slot_auth.clone());
    let runtime = Arc::new(AccountRuntimeBundle {
        auth_manager: slot_auth,
        models_manager: slot_models,
    });
    let installed = registry
        .state
        .read()
        .expect("registry state")
        .slots
        .iter()
        .find(|slot| slot.manifest.account_slot_id == SECOND_SLOT_ID)
        .expect("secondary slot")
        .runtime
        .set(runtime);
    assert!(installed.is_ok());
    let ready = registry
        .slot_snapshot(SECOND_SLOT_ID)
        .await
        .expect("ready slot snapshot");
    assert_eq!(
        ready
            .actions
            .iter()
            .find(|action| action.action == AccountSlotAction::SwitchTo),
        Some(&AccountSlotActionAvailability {
            action: AccountSlotAction::SwitchTo,
            allowed: false,
            deny_reason: Some(DENY_SWITCH_NOT_AVAILABLE.to_string()),
        })
    );

    let logged_out = registry
        .logout_secondary(AccountSlotLogoutParams {
            account_slot_id: SECOND_SLOT_ID.to_string(),
            expected_registry_revision: 7,
            expected_attempt_generation: 2,
        })
        .await
        .expect("logout secondary slot");

    assert_eq!(
        logged_out.response.slot.status,
        AccountSlotStatus::LoginRequired
    );
    assert_eq!(logged_out.response.slot.attempt_generation, 3);
    assert_eq!(logged_out.response.slot.registry_revision, 8);
}

#[tokio::test]
async fn secondary_logout_rejects_default_stale_and_active_login() {
    let process_home = tempdir().expect("temp process home");
    let registry = registry_for_home(process_home.path()).await;
    assert!(
        registry
            .logout_secondary(AccountSlotLogoutParams {
                account_slot_id: DEFAULT_SLOT_ID.to_string(),
                expected_registry_revision: 1,
                expected_attempt_generation: 0,
            })
            .await
            .is_err()
    );
    let prepared = registry
        .prepare_slot_login(/*requested_slot_id*/ None, "operation-1".to_string())
        .await
        .expect("prepare slot");
    let snapshot = registry
        .slot_snapshot(&prepared.account_slot_id)
        .await
        .expect("slot snapshot");
    assert!(
        registry
            .logout_secondary(AccountSlotLogoutParams {
                account_slot_id: prepared.account_slot_id.clone(),
                expected_registry_revision: snapshot.registry_revision.saturating_sub(1),
                expected_attempt_generation: prepared.attempt_generation,
            })
            .await
            .is_err()
    );
    let snapshot = registry
        .slot_snapshot(&prepared.account_slot_id)
        .await
        .expect("refreshed slot snapshot");
    let active = match registry
        .logout_secondary(AccountSlotLogoutParams {
            account_slot_id: prepared.account_slot_id,
            expected_registry_revision: snapshot.registry_revision,
            expected_attempt_generation: prepared.attempt_generation,
        })
        .await
    {
        Ok(_) => panic!("active login must reject logout"),
        Err(error) => error,
    };
    assert_eq!(
        active.data,
        Some(serde_json::json!({"reason":"accountSlotLoginBusy"}))
    );
}

#[tokio::test]
async fn logout_reservation_is_exact_and_allows_other_slot_mutation() {
    let process_home = tempdir().expect("temp process home");
    let mut slots = manifest(process_home.path(), 7);
    slots.slots[1].status = ManifestSlotStatus::Ready;
    slots.slots.push(AccountSlotManifest {
        account_slot_id: THIRD_SLOT_ID.to_string(),
        label: "Third account".to_string(),
        auth_home: process_home
            .path()
            .join(PRIVATE_HOMES_DIR)
            .join(THIRD_SLOT_ID),
        is_default: false,
        status: ManifestSlotStatus::LoginRequired,
        attempt_generation: 1,
        updated_at: 30,
        error_code: None,
    });
    slots
        .persist(&process_home.path().join(MANIFEST_FILE))
        .expect("persist manifest");
    let registry = registry_for_home(process_home.path()).await;
    let params = || AccountSlotLogoutParams {
        account_slot_id: SECOND_SLOT_ID.to_string(),
        expected_registry_revision: 7,
        expected_attempt_generation: 2,
    };

    let first = registry
        .reserve_secondary_logout(params())
        .await
        .expect("reserve first logout");
    let duplicate = match registry.reserve_secondary_logout(params()).await {
        Ok(_) => panic!("duplicate reservation must fail"),
        Err(error) => error,
    };
    assert_eq!(
        duplicate.data,
        Some(serde_json::json!({"reason":"accountSlotLogoutBusy"}))
    );

    registry
        .clear_logout_reservation(&first)
        .await
        .expect("clear first reservation");
    let second = registry
        .reserve_secondary_logout(params())
        .await
        .expect("reserve second logout");
    registry
        .clear_logout_reservation(&first)
        .await
        .expect("stale clear is harmless");
    assert!(registry.reserve_secondary_logout(params()).await.is_err());

    registry
        .prepare_slot_login(
            Some(THIRD_SLOT_ID.to_string()),
            "other-slot-login".to_string(),
        )
        .await
        .expect("other slot mutation proceeds");
    assert!(
        registry
            .state
            .read()
            .expect("registry state")
            .slots
            .iter()
            .find(|slot| slot.manifest.account_slot_id == SECOND_SLOT_ID)
            .expect("secondary slot")
            .active_logout_operation_id
            .is_some()
    );
    registry
        .clear_logout_reservation(&second)
        .await
        .expect("clear second reservation");
}

#[tokio::test]
async fn logout_reservation_blocks_only_the_target_slot_resolver() {
    let process_home = tempdir().expect("temp process home");
    let mut slots = manifest(process_home.path(), 7);
    slots.slots[1].status = ManifestSlotStatus::Ready;
    slots
        .persist(&process_home.path().join(MANIFEST_FILE))
        .expect("persist manifest");
    let registry = registry_for_home(process_home.path()).await;
    let slot_auth = AuthManager::from_auth_for_testing_with_home(
        CodexAuth::from_api_key("slot-key"),
        slots.slots[1].auth_home.clone(),
    );
    let slot_models = codex_core::build_models_manager(registry.config.as_ref(), slot_auth.clone());
    let runtime = Arc::new(AccountRuntimeBundle {
        auth_manager: slot_auth,
        models_manager: slot_models,
    });
    let installed = registry
        .state
        .read()
        .expect("registry state")
        .slots
        .iter()
        .find(|slot| slot.manifest.account_slot_id == SECOND_SLOT_ID)
        .expect("secondary slot")
        .runtime
        .set(runtime);
    assert!(installed.is_ok());
    let reservation = registry
        .reserve_secondary_logout(AccountSlotLogoutParams {
            account_slot_id: SECOND_SLOT_ID.to_string(),
            expected_registry_revision: 7,
            expected_attempt_generation: 2,
        })
        .await
        .expect("reserve logout");

    assert!(
        registry
            .resolve(ExecutionAccountBinding {
                slot_id: SECOND_SLOT_ID.to_string(),
                generation: 1,
            })
            .await
            .is_err()
    );
    assert!(
        registry
            .resolve(ExecutionAccountBinding {
                slot_id: DEFAULT_SLOT_ID.to_string(),
                generation: 1,
            })
            .await
            .is_ok()
    );

    registry
        .clear_logout_reservation(&reservation)
        .await
        .expect("clear reservation");
}
