use std::fs;
use std::path::Path;
use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::COORDINATION_LOCK_FILE;
use super::ThreadWriterOwnership;
use super::WRITER_LOCK_DIR;
use super::WriterControlCapability;
use super::WriterLockCoordinator;
use crate::ThreadStoreError;
use crate::local::LocalThreadStore;
use crate::local::LocalThreadStoreConfig;

fn store_config(codex_home: &Path, sqlite_home: &Path) -> LocalThreadStoreConfig {
    LocalThreadStoreConfig {
        codex_home: codex_home.to_path_buf(),
        sqlite: codex_state::SqliteConfig::new_for_testing(sqlite_home.abs()),
        default_model_provider_id: "test-provider".to_string(),
    }
}

#[test]
fn writer_locks_reject_competing_owners_and_release_their_files() {
    let home = TempDir::new().expect("temp dir");
    let primary = Arc::new(WriterLockCoordinator::new(home.path()));
    let secondary = Arc::new(WriterLockCoordinator::new(home.path()));
    let thread_id = ThreadId::default();
    let other_thread_id = ThreadId::default();

    let owner = primary.acquire(thread_id).expect("acquire writer lock");
    let lock_path = home
        .path()
        .join(WRITER_LOCK_DIR)
        .join(format!("{thread_id}.lock"));
    assert!(lock_path.exists());

    let err = match secondary.acquire(thread_id) {
        Ok(_) => panic!("competing owner should fail"),
        Err(err) => err,
    };
    assert!(matches!(err, ThreadStoreError::Conflict { .. }));
    let other_owner = secondary
        .acquire(other_thread_id)
        .expect("other thread should acquire its own lock");

    drop(owner);
    assert!(!lock_path.exists());
    let next_owner = secondary
        .acquire(thread_id)
        .expect("released thread should accept another owner");
    drop(next_owner);
    drop(other_owner);

    let entries = fs::read_dir(home.path().join(WRITER_LOCK_DIR))
        .expect("read lock directory")
        .map(|entry| entry.expect("lock directory entry").file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![COORDINATION_LOCK_FILE]);
}

#[test]
fn first_acquisition_removes_stale_locks_without_removing_active_locks() {
    let home = TempDir::new().expect("temp dir");
    let primary = Arc::new(WriterLockCoordinator::new(home.path()));
    let active_thread_id = ThreadId::default();
    let active_owner = primary
        .acquire(active_thread_id)
        .expect("acquire active writer lock");

    let stale_thread_id = ThreadId::default();
    let stale_path = home
        .path()
        .join(WRITER_LOCK_DIR)
        .join(format!("{stale_thread_id}.lock"));
    fs::File::create(&stale_path).expect("create stale writer lock");

    let secondary = Arc::new(WriterLockCoordinator::new(home.path()));
    let secondary_owner = secondary
        .acquire(ThreadId::default())
        .expect("acquire writer lock after cleanup");

    assert!(!stale_path.exists());
    let err = match secondary.acquire(active_thread_id) {
        Ok(_) => panic!("active writer should survive cleanup"),
        Err(err) => err,
    };
    assert!(matches!(err, ThreadStoreError::Conflict { .. }));

    drop(secondary_owner);
    drop(active_owner);
}

#[tokio::test]
async fn different_account_homes_share_writer_authority_through_sqlite_home() {
    let root = TempDir::new().expect("temp dir");
    let sqlite_home = root.path().join("shared-sqlite");
    let first_home = root.path().join("account-a");
    let second_home = root.path().join("account-b");
    fs::create_dir_all(&first_home).expect("create first account home");
    fs::create_dir_all(&second_home).expect("create second account home");
    let runtime = codex_state::StateRuntime::init(
        codex_state::SqliteConfig::new_for_testing(sqlite_home.as_path().abs()),
        "test-provider".to_string(),
    )
    .await
    .expect("initialize state runtime");
    let primary = LocalThreadStore::new(
        store_config(&first_home, &sqlite_home),
        Some(runtime.clone()),
    );
    let secondary = LocalThreadStore::new(
        store_config(&second_home, &sqlite_home),
        Some(runtime.clone()),
    );
    let thread_id = ThreadId::default();

    let first_guard = primary
        .acquire_writer_lock(thread_id)
        .await
        .expect("acquire first writer");
    assert!(matches!(
        secondary.acquire_writer_lock(thread_id).await,
        Err(ThreadStoreError::Conflict { .. })
    ));
    assert_eq!(
        primary
            .probe_writer_authority(thread_id)
            .await
            .expect("probe primary authority"),
        super::ThreadWriterAuthority {
            ownership: ThreadWriterOwnership::OwnedHere,
            generation: Some(1),
            control: WriterControlCapability::Enabled {
                store_id: runtime.writer_store_id().to_string(),
            },
        }
    );
    assert_eq!(
        secondary
            .probe_writer_authority(thread_id)
            .await
            .expect("probe secondary authority")
            .ownership,
        ThreadWriterOwnership::OwnedElsewhere
    );

    drop(first_guard);
    let second_guard = secondary
        .acquire_writer_lock(thread_id)
        .await
        .expect("acquire next writer");
    assert_eq!(
        secondary
            .probe_writer_authority(thread_id)
            .await
            .expect("probe next authority")
            .generation,
        Some(2)
    );
    drop(second_guard);
    runtime.close().await;
}

#[tokio::test]
async fn missing_state_db_keeps_shared_filesystem_authority_but_disables_control() {
    let root = TempDir::new().expect("temp dir");
    let sqlite_home = root.path().join("shared-sqlite");
    let primary = LocalThreadStore::new(
        store_config(&root.path().join("account-a"), &sqlite_home),
        /*state_db*/ None,
    );
    let secondary = LocalThreadStore::new(
        store_config(&root.path().join("account-b"), &sqlite_home),
        /*state_db*/ None,
    );
    let thread_id = ThreadId::default();

    let owner = primary
        .acquire_writer_lock(thread_id)
        .await
        .expect("acquire filesystem writer");
    assert!(matches!(
        secondary.acquire_writer_lock(thread_id).await,
        Err(ThreadStoreError::Conflict { .. })
    ));
    let authority = secondary
        .probe_writer_authority(thread_id)
        .await
        .expect("probe sqlite-less authority");
    assert_eq!(authority.ownership, ThreadWriterOwnership::OwnedElsewhere);
    assert_eq!(authority.generation, None);
    assert!(matches!(
        authority.control,
        WriterControlCapability::Disabled { reason }
            if reason == "persistent writer control requires the state database"
    ));
    drop(owner);
}

#[tokio::test]
async fn authority_probe_does_not_create_lock_storage() {
    let root = TempDir::new().expect("temp dir");
    let sqlite_home = root.path().join("shared-sqlite");
    let store = LocalThreadStore::new(
        store_config(&root.path().join("account-a"), &sqlite_home),
        /*state_db*/ None,
    );

    let authority = store
        .probe_writer_authority(ThreadId::default())
        .await
        .expect("probe absent writer");

    assert_eq!(authority.ownership, ThreadWriterOwnership::None);
    assert!(!sqlite_home.join(WRITER_LOCK_DIR).exists());
}

#[tokio::test]
async fn different_sqlite_homes_have_independent_writer_authority() {
    let root = TempDir::new().expect("temp dir");
    let primary = LocalThreadStore::new(
        store_config(
            &root.path().join("account-a"),
            &root.path().join("sqlite-a"),
        ),
        /*state_db*/ None,
    );
    let secondary = LocalThreadStore::new(
        store_config(
            &root.path().join("account-b"),
            &root.path().join("sqlite-b"),
        ),
        /*state_db*/ None,
    );
    let thread_id = ThreadId::default();

    let primary_owner = primary
        .acquire_writer_lock(thread_id)
        .await
        .expect("acquire first sqlite authority");
    let secondary_owner = secondary
        .acquire_writer_lock(thread_id)
        .await
        .expect("acquire independent sqlite authority");

    drop(secondary_owner);
    drop(primary_owner);
}

#[tokio::test]
async fn single_home_keeps_the_existing_writer_lock_path() {
    let home = TempDir::new().expect("temp dir");
    let store = LocalThreadStore::new(
        store_config(home.path(), home.path()),
        /*state_db*/ None,
    );
    let thread_id = ThreadId::default();

    let owner = store
        .acquire_writer_lock(thread_id)
        .await
        .expect("acquire single-home writer");
    let lock_path = home
        .path()
        .join(WRITER_LOCK_DIR)
        .join(format!("{thread_id}.lock"));
    assert!(lock_path.exists());
    drop(owner);
    assert!(!lock_path.exists());
}
