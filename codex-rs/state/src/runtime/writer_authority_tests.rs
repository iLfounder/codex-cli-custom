use codex_protocol::ThreadId;
use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;

use crate::SqliteConfig;
use crate::StateRuntime;

#[tokio::test]
async fn writer_store_identity_and_generation_survive_runtime_restart() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("create sqlite home");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let thread_id = ThreadId::default();

    let first = StateRuntime::init(sqlite.clone(), "test-provider".to_string())
        .await
        .expect("initialize first runtime");
    let custom_schema_versions = sqlx::query_scalar::<_, i64>(
        "SELECT version FROM codex_custom_schema_migrations ORDER BY version",
    )
    .fetch_all(first.pool.as_ref())
    .await
    .expect("read custom schema versions");
    assert_eq!(custom_schema_versions, vec![1, 2, 3, 4]);
    let first_generation = first
        .next_writer_generation(thread_id)
        .await
        .expect("allocate first generation");
    assert_eq!(first_generation.generation, 1);
    first.close().await;
    drop(first);

    let restarted = StateRuntime::init(sqlite, "test-provider".to_string())
        .await
        .expect("initialize restarted runtime");
    assert_eq!(restarted.writer_store_id(), first_generation.store_id);
    assert_eq!(
        restarted
            .writer_generation(thread_id)
            .await
            .expect("read persisted generation"),
        Some(first_generation.clone())
    );
    assert_eq!(
        restarted
            .next_writer_generation(thread_id)
            .await
            .expect("allocate next generation"),
        super::WriterGeneration {
            store_id: first_generation.store_id,
            generation: 2,
        }
    );
    restarted.close().await;
}
