use super::*;
use codex_core::config::ConfigBuilder;
use codex_login::CodexAuth;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

const SECOND_SLOT_ID: &str = "11111111111141118111111111111111";

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
