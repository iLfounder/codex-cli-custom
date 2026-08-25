use super::*;
use codex_app_server_protocol::AccountSlotAction;
use codex_app_server_protocol::AccountSlotActionAvailability;
use codex_app_server_protocol::AccountSlotLogoutParams;
use codex_config::types::AuthCredentialsStoreMode;
use codex_core::config::ConfigBuilder;
use codex_login::AuthKeyringBackendKind;
use codex_login::CodexAuth;
use codex_login::login_with_api_key;
use codex_login::logout;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

const SECOND_SLOT_ID: &str = "11111111111141118111111111111111";
const THIRD_SLOT_ID: &str = "22222222222242228222222222222222";

fn persist_api_key_auth(auth_home: &Path) {
    login_with_api_key(
        auth_home,
        "slot-key",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .expect("persist slot auth");
}

async fn registry_for_home(process_home: &Path) -> AccountRegistry {
    registry_for_home_and_store(
        process_home,
        Arc::new(codex_thread_store::InMemoryThreadStore::default()),
    )
    .await
}

async fn registry_for_home_and_store(
    process_home: &Path,
    thread_store: Arc<dyn codex_thread_store::ThreadStore>,
) -> AccountRegistry {
    login_with_api_key(
        process_home,
        "dummy",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .expect("persist default auth");
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
    let config_manager = ConfigManager::new(
        process_home.to_path_buf(),
        Vec::new(),
        codex_config::LoaderOverrides::default(),
        /*strict_config*/ false,
        codex_config::CloudConfigBundleLoader::default(),
        codex_arg0::Arg0DispatchPaths::default(),
        Arc::new(codex_config::NoopThreadConfigLoader),
    );
    AccountRegistry::new(
        config,
        config_manager,
        auth_manager,
        models_manager,
        thread_store,
    )
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
        .prepare_slot_login(
            /*requested_slot_id*/ None,
            "operation-1".to_string(),
            /*candidate_runtime_version*/ 1,
        )
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
async fn successful_terminal_claim_blocks_cancel_and_overlapping_attempt() {
    let process_home = tempdir().expect("temp process home");
    let registry = registry_for_home(process_home.path()).await;
    let prepared = registry
        .prepare_slot_login(
            /*requested_slot_id*/ None,
            "operation-1".to_string(),
            /*candidate_runtime_version*/ 1,
        )
        .await
        .expect("prepare slot");

    assert!(prepared.try_claim_success());
    assert!(!prepared.clone().try_claim_failure());
    let overlapping = match registry
        .prepare_slot_login(
            Some(prepared.account_slot_id.clone()),
            "operation-2".to_string(),
            /*candidate_runtime_version*/ 1,
        )
        .await
    {
        Ok(_) => panic!("active terminal owner must block candidate reuse"),
        Err(error) => error,
    };
    assert_eq!(
        overlapping.data,
        Some(serde_json::json!({"reason":"accountSlotLoginBusy"}))
    );

    registry
        .finish_slot_login(&prepared, ManifestSlotStatus::Ready, None)
        .await
        .expect("publish ready slot")
        .expect("active attempt must publish");
    assert!(!prepared.try_claim_failure());
    let snapshot = registry
        .slot_snapshot(&prepared.account_slot_id)
        .await
        .expect("slot snapshot");
    assert_eq!(snapshot.status, AccountSlotStatus::Ready);
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
async fn default_login_projection_uses_exact_attempt_cas_and_exposes_cancel() {
    let process_home = tempdir().expect("temp process home");
    let registry = registry_for_home(process_home.path()).await;
    let (first, first_started) = registry
        .prepare_default_login("default-login-1".to_string())
        .await
        .expect("prepare first default login");
    assert_eq!(first_started.slot.active_login_operation_id, None,);
    let first_cancelable = registry
        .mark_login_cancelable("default", first.attempt_generation, &first.operation_id)
        .await
        .expect("mark first login cancelable");
    assert_eq!(
        first_cancelable.slot.active_login_operation_id.as_deref(),
        Some("default-login-1")
    );

    let (second, second_started) = registry
        .prepare_default_login("default-login-2".to_string())
        .await
        .expect("replace default login");
    assert_eq!(second_started.slot.active_login_operation_id, None);
    let second_started = registry
        .mark_login_cancelable("default", second.attempt_generation, &second.operation_id)
        .await
        .expect("mark second login cancelable");
    assert_eq!(
        (
            second_started.slot.active_login_operation_id.as_deref(),
            second_started.slot.attempt_generation,
        ),
        (Some("default-login-2"), first.attempt_generation + 1)
    );
    assert_eq!(
        registry
            .finish_default_login(&first, true, None)
            .await
            .expect("ignore superseded completion"),
        None
    );

    assert!(second.try_claim_failure());
    let canceled = registry
        .finish_default_login(
            &second,
            false,
            Some(live_registration::ERROR_LOGIN_CANCELED),
        )
        .await
        .expect("cancel default login")
        .expect("matching attempt must publish");
    assert_eq!(
        (
            canceled.slot.status,
            canceled.slot.active_login_operation_id,
            canceled.slot.error_code.as_deref(),
        ),
        (
            AccountSlotStatus::Ready,
            None,
            Some(live_registration::ERROR_LOGIN_CANCELED),
        )
    );
}

#[tokio::test]
async fn default_projection_refresh_preserves_active_login_cas() {
    let process_home = tempdir().expect("temp process home");
    let registry = registry_for_home(process_home.path()).await;
    let (prepared, _) = registry
        .prepare_default_login("default-login".to_string())
        .await
        .expect("prepare default login");
    let started = registry
        .mark_login_cancelable(
            "default",
            prepared.attempt_generation,
            &prepared.operation_id,
        )
        .await
        .expect("mark login cancelable");

    let refreshed = registry
        .refresh_default_projection()
        .await
        .expect("refresh default projection");
    assert_eq!(
        (
            refreshed.slot.status,
            refreshed.slot.active_login_operation_id.as_deref(),
            refreshed.slot.attempt_generation,
            refreshed.slot.registry_revision,
        ),
        (
            AccountSlotStatus::Ready,
            Some("default-login"),
            prepared.attempt_generation,
            started.slot.registry_revision + 1,
        )
    );

    assert!(prepared.try_claim_failure());
    let terminal = registry
        .finish_default_login(
            &prepared,
            false,
            Some(live_registration::ERROR_LOGIN_CANCELED),
        )
        .await
        .expect("finish default login")
        .expect("active login must retain its terminal CAS");
    assert_eq!(
        (
            terminal.slot.status,
            terminal.slot.active_login_operation_id,
            terminal.slot.error_code.as_deref(),
        ),
        (
            AccountSlotStatus::Ready,
            None,
            Some(live_registration::ERROR_LOGIN_CANCELED),
        )
    );
}

#[tokio::test]
async fn committed_default_login_clears_active_projection_when_manifest_write_is_delayed() {
    let process_home = tempdir().expect("temp process home");
    let registry = registry_for_home(process_home.path()).await;
    let (prepared, _) = registry
        .prepare_default_login("default-login".to_string())
        .await
        .expect("prepare default login");
    registry
        .mark_login_cancelable(
            DEFAULT_SLOT_ID,
            prepared.attempt_generation,
            &prepared.operation_id,
        )
        .await
        .expect("mark login cancelable");
    assert!(prepared.try_begin_credential_commit());
    prepared.finish_credential_commit(true);

    let manifest_path = process_home.path().join(MANIFEST_FILE);
    std::fs::remove_file(&manifest_path).expect("remove manifest fixture");
    std::fs::create_dir(&manifest_path).expect("block manifest replacement");

    let terminal = registry
        .finish_default_login(&prepared, true, None)
        .await
        .expect("committed auth must retain an in-memory terminal projection")
        .expect("matching attempt must publish");
    assert_eq!(
        (
            terminal.slot.status,
            terminal.slot.active_login_operation_id,
            terminal.slot.error_code,
        ),
        (AccountSlotStatus::Ready, None, None)
    );
    assert!(
        registry
            .state
            .read()
            .expect("read registry state")
            .projection_dirty
    );

    std::fs::remove_dir(&manifest_path).expect("unblock manifest replacement");
    registry
        .reconcile()
        .await
        .expect("retry manifest projection");
    assert!(
        !registry
            .state
            .read()
            .expect("read registry state")
            .projection_dirty
    );
}

#[tokio::test]
async fn reconcile_reloads_added_and_removed_auth_once() {
    let process_home = tempdir().expect("temp process home");
    let slots = manifest(process_home.path(), 1);
    let secondary_home = slots.slots[1].auth_home.clone();
    slots
        .persist(&process_home.path().join(MANIFEST_FILE))
        .expect("persist manifest");
    let registry = registry_for_home(process_home.path()).await;

    let initial = registry
        .slot_snapshot(SECOND_SLOT_ID)
        .await
        .expect("initial slot snapshot");
    let auth_manager = registry
        .state
        .read()
        .expect("registry state")
        .slots
        .iter()
        .find(|slot| slot.manifest.account_slot_id == SECOND_SLOT_ID)
        .and_then(|slot| slot.runtime.get())
        .map(|runtime| Arc::clone(&runtime.auth_manager))
        .expect("initialized slot auth manager");
    let auth_changes = auth_manager.auth_change_receiver();
    assert_eq!(
        (
            initial.status,
            initial.registry_revision,
            *auth_changes.borrow()
        ),
        (AccountSlotStatus::LoginRequired, 1, 0)
    );

    login_with_api_key(
        &secondary_home,
        "slot-key",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .expect("persist secondary auth");
    let mut state = registry.state.write().expect("registry state");
    let slot = state
        .slots
        .iter_mut()
        .find(|slot| slot.manifest.account_slot_id == SECOND_SLOT_ID)
        .expect("secondary slot");
    slot.active_login_operation_id = Some("active-login".to_string());
    slot.active_login_cancelable = true;
    drop(state);
    registry.reconcile().await.expect("busy reconcile");
    let busy = registry
        .slot_snapshot(SECOND_SLOT_ID)
        .await
        .expect("busy slot snapshot");
    assert_eq!(
        (
            busy.status,
            busy.registry_revision,
            busy.active_login_operation_id.as_deref(),
            *auth_changes.borrow(),
        ),
        (
            AccountSlotStatus::LoginRequired,
            initial.registry_revision,
            Some("active-login"),
            0,
        )
    );
    let mut state = registry.state.write().expect("registry state");
    let slot = state
        .slots
        .iter_mut()
        .find(|slot| slot.manifest.account_slot_id == SECOND_SLOT_ID)
        .expect("secondary slot");
    slot.active_login_operation_id = None;
    slot.active_login_cancelable = false;
    drop(state);

    registry.reconcile().await.expect("reconcile");
    let ready = registry
        .slot_snapshot(SECOND_SLOT_ID)
        .await
        .expect("ready slot snapshot");
    assert_eq!(
        (
            ready.status,
            ready.registry_revision,
            *auth_changes.borrow()
        ),
        (AccountSlotStatus::Ready, 2, 1)
    );

    registry.reconcile().await.expect("stable ready reconcile");
    let unchanged_ready = registry
        .slot_snapshot(SECOND_SLOT_ID)
        .await
        .expect("unchanged ready slot snapshot");
    assert_eq!(unchanged_ready, ready);
    assert_eq!(*auth_changes.borrow(), 1);

    assert!(
        logout(
            &secondary_home,
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default(),
        )
        .expect("remove secondary auth")
    );
    registry.reconcile().await.expect("missing auth reconcile");
    let failed = registry
        .slot_snapshot(SECOND_SLOT_ID)
        .await
        .expect("failed slot snapshot");
    assert_eq!(
        (
            failed.status,
            failed.error_code.as_deref(),
            failed.registry_revision,
            *auth_changes.borrow(),
        ),
        (AccountSlotStatus::Failed, Some("authUnavailable"), 3, 2)
    );

    registry.reconcile().await.expect("stable failed reconcile");
    let unchanged_failed = registry
        .slot_snapshot(SECOND_SLOT_ID)
        .await
        .expect("unchanged failed slot snapshot");
    assert_eq!(unchanged_failed, failed);
    assert_eq!(*auth_changes.borrow(), 2);
}

#[tokio::test]
async fn secondary_logout_revokes_exact_ready_slot_and_bumps_generation() {
    let process_home = tempdir().expect("temp process home");
    let mut slots = manifest(process_home.path(), 7);
    slots.slots[1].status = ManifestSlotStatus::Ready;
    persist_api_key_auth(&slots.slots[1].auth_home);
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
        runtime_version: AtomicU64::new(0),
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
            allowed: true,
            deny_reason: None,
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
        .prepare_slot_login(
            /*requested_slot_id*/ None,
            "operation-1".to_string(),
            /*candidate_runtime_version*/ 1,
        )
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
    persist_api_key_auth(&slots.slots[1].auth_home);
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
    drop(first);
    let second = registry
        .reserve_secondary_logout(params())
        .await
        .expect("reserve second logout");
    assert!(registry.reserve_secondary_logout(params()).await.is_err());

    registry
        .prepare_slot_login(
            Some(THIRD_SLOT_ID.to_string()),
            "other-slot-login".to_string(),
            /*candidate_runtime_version*/ 1,
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
    persist_api_key_auth(&slots.slots[1].auth_home);
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
        runtime_version: AtomicU64::new(0),
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

#[tokio::test]
async fn transition_resolution_holds_target_readiness_lease() {
    let process_home = tempdir().expect("temp process home");
    let mut slots = manifest(process_home.path(), 7);
    slots.slots[1].status = ManifestSlotStatus::Ready;
    persist_api_key_auth(&slots.slots[1].auth_home);
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
        runtime_version: AtomicU64::new(0),
        auth_manager: slot_auth,
        models_manager: slot_models,
    });
    let binding_transition = {
        let state = registry.state.read().expect("registry state");
        let slot = state
            .slots
            .iter()
            .find(|slot| slot.manifest.account_slot_id == SECOND_SLOT_ID)
            .expect("secondary slot");
        assert!(slot.runtime.set(runtime).is_ok());
        Arc::clone(&slot.binding_transition)
    };

    let transition = registry
        .resolve_for_transition(ExecutionAccountBinding {
            slot_id: SECOND_SLOT_ID.to_string(),
            generation: 1,
        })
        .await
        .expect("resolve transition");

    assert!(Arc::clone(&binding_transition).try_lock_owned().is_err());
    drop(transition);
    assert!(binding_transition.try_lock_owned().is_ok());
}

#[tokio::test]
async fn restart_resolves_runtime_home_from_durable_slot_version() {
    let process_home = tempdir().expect("temp process home");
    let mut slots = manifest(process_home.path(), 7);
    slots.slots[1].status = ManifestSlotStatus::Failed;
    slots
        .persist(&process_home.path().join(MANIFEST_FILE))
        .expect("persist recovery projection");
    let thread_store: Arc<dyn codex_thread_store::ThreadStore> =
        Arc::new(codex_thread_store::InMemoryThreadStore::default());
    assert_eq!(
        thread_store
            .compare_and_swap_execution_account_slot_runtime(
                SECOND_SLOT_ID.to_string(),
                /*expected_runtime_version*/ 0,
                Vec::new(),
            )
            .await
            .expect("commit runtime version"),
        Some((1, Vec::new()))
    );
    persist_api_key_auth(
        &process_home
            .path()
            .join(PRIVATE_HOMES_DIR)
            .join(SECOND_SLOT_ID)
            .join("runtime-1"),
    );
    let registry = registry_for_home_and_store(process_home.path(), thread_store).await;

    registry
        .reconcile()
        .await
        .expect("reconcile durable runtime");

    let state = registry.state.read().expect("registry state");
    let slot = state
        .slots
        .iter()
        .find(|slot| slot.manifest.account_slot_id == SECOND_SLOT_ID)
        .expect("secondary slot");
    assert_eq!(
        (&slot.manifest.auth_home, slot.manifest.status),
        (
            &process_home
                .path()
                .join(PRIVATE_HOMES_DIR)
                .join(SECOND_SLOT_ID)
                .join("runtime-1"),
            ManifestSlotStatus::Ready,
        )
    );
}
