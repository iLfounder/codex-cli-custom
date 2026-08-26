use codex_utils_absolute_path::test_support::PathExt;
use pretty_assertions::assert_eq;
use sqlx::Connection;
use sqlx::Row;
use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;
use std::borrow::Cow;

use super::CUSTOM_SCHEMA_V1;
use super::CUSTOM_SCHEMA_V2;
use super::CUSTOM_SCHEMA_V3;
use super::CUSTOM_SCHEMA_V4;
use super::CUSTOM_SCHEMA_V5;
use super::LegacyMigrationCutover;
use super::STATE_MIGRATOR;
use super::THREAD_HISTORY_MIGRATOR;
use super::apply_custom_schema_migrations;
use super::migrate_legacy_custom_schema_migrations;
use super::repair_legacy_recency_migration_version;
use super::runtime_state_migrator;
use crate::PINNED_THREAD_SECTION_ID;
use crate::PINNED_THREAD_SECTION_NAME;

const CUSTOM_THREAD_SECTION_ID: &str = "01984de2-8f74-7c91-a3b2-5c5e937cf317";

fn migrator_through(version: i64) -> Migrator {
    Migrator {
        migrations: Cow::Owned(
            STATE_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version <= version)
                .cloned()
                .collect(),
        ),
        ignore_missing: STATE_MIGRATOR.ignore_missing,
        locking: STATE_MIGRATOR.locking,
        table_name: STATE_MIGRATOR.table_name.clone(),
        create_schemas: STATE_MIGRATOR.create_schemas.clone(),
        no_tx: STATE_MIGRATOR.no_tx,
    }
}

fn decode_checksum(checksum: &str) -> Vec<u8> {
    (0..checksum.len())
        .step_by(2)
        .map(|offset| {
            u8::from_str_radix(&checksum[offset..offset + 2], 16)
                .expect("checksum should be hexadecimal")
        })
        .collect()
}

async fn insert_legacy_custom_migration(
    pool: &sqlx::SqlitePool,
    migration: &super::CustomSchemaMigration,
    checksum: &str,
) {
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, installed_on, success, checksum, execution_time) \
         VALUES (?, ?, CURRENT_TIMESTAMP, TRUE, ?, 0)",
    )
    .bind(migration.legacy_upstream_version)
    .bind(migration.legacy_description)
    .bind(decode_checksum(checksum))
    .execute(pool)
    .await
    .expect("legacy migration row should insert");
}

#[tokio::test]
async fn custom_schema_bootstrap_with_account_slot_runtime_versions_serializes_concurrent_callers()
{
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("database should open");

    let (first, second) = tokio::join!(
        apply_custom_schema_migrations(&pool),
        apply_custom_schema_migrations(&pool)
    );
    first.expect("first custom bootstrap should apply");
    second.expect("second custom bootstrap should observe the applied schema");

    let applied = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT version, name, definition FROM codex_custom_schema_migrations",
    )
    .fetch_all(&pool)
    .await
    .expect("custom migration record should load");
    assert_eq!(
        applied,
        vec![
            (
                1,
                "writer_authority".to_string(),
                CUSTOM_SCHEMA_V1.definition.to_string(),
            ),
            (
                2,
                "execution_account_bindings".to_string(),
                CUSTOM_SCHEMA_V2.definition.to_string(),
            ),
            (
                3,
                "account_slot_runtime_versions".to_string(),
                CUSTOM_SCHEMA_V3.definition.to_string(),
            ),
            (
                4,
                "thread_transitions".to_string(),
                CUSTOM_SCHEMA_V4.definition.to_string(),
            ),
            (
                5,
                "thread_account_rotation_policies".to_string(),
                CUSTOM_SCHEMA_V5.definition.to_string(),
            ),
        ]
    );
    let custom_tables = sqlx::query_scalar::<_, String>(
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name IN ('writer_authority_store', 'thread_writer_generations', \
         'thread_execution_account_bindings', 'thread_turn_execution_accounts', \
         'account_slot_runtime_versions', 'thread_transitions', \
         'thread_account_rotation_policies') ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .expect("custom tables should load");
    assert_eq!(
        custom_tables,
        vec![
            "account_slot_runtime_versions".to_string(),
            "thread_account_rotation_policies".to_string(),
            "thread_execution_account_bindings".to_string(),
            "thread_transitions".to_string(),
            "thread_turn_execution_accounts".to_string(),
            "thread_writer_generations".to_string(),
            "writer_authority_store".to_string(),
        ]
    );

    pool.close().await;
}

#[tokio::test]
async fn legacy_v49_and_v50_require_opt_in_then_cut_over_without_losing_data() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("database should open");
    migrator_through(/*version*/ 48)
        .run(&pool)
        .await
        .expect("upstream migrations through 48 should apply");
    sqlx::raw_sql(CUSTOM_SCHEMA_V1.definition)
        .execute(&pool)
        .await
        .expect("legacy writer schema should apply");
    sqlx::raw_sql(CUSTOM_SCHEMA_V2.definition)
        .execute(&pool)
        .await
        .expect("legacy execution binding schema should apply");
    insert_legacy_custom_migration(&pool, &CUSTOM_SCHEMA_V1, CUSTOM_SCHEMA_V1.legacy_checksum)
        .await;
    insert_legacy_custom_migration(&pool, &CUSTOM_SCHEMA_V2, CUSTOM_SCHEMA_V2.legacy_checksum)
        .await;
    sqlx::query("INSERT INTO writer_authority_store (singleton, store_id) VALUES (1, ?)")
        .bind("legacy-store")
        .execute(&pool)
        .await
        .expect("legacy store identity should insert");
    sqlx::query("INSERT INTO thread_writer_generations (thread_id, generation) VALUES (?, ?)")
        .bind("00000000-0000-0000-0000-000000000049")
        .bind(7_i64)
        .execute(&pool)
        .await
        .expect("legacy writer generation should insert");
    sqlx::query(
        "INSERT INTO thread_execution_account_bindings \
         (thread_id, slot_id, generation) VALUES (?, ?, ?)",
    )
    .bind("00000000-0000-0000-0000-000000000049")
    .bind("account-a")
    .bind(3_i64)
    .execute(&pool)
    .await
    .expect("legacy execution binding should insert");
    sqlx::query(
        "INSERT INTO thread_turn_execution_accounts \
         (thread_id, turn_id, slot_id, generation) VALUES (?, ?, ?, ?)",
    )
    .bind("00000000-0000-0000-0000-000000000049")
    .bind("turn-1")
    .bind("account-a")
    .bind(3_i64)
    .execute(&pool)
    .await
    .expect("legacy turn provenance should insert");
    let state_migrator = runtime_state_migrator();
    let error = migrate_legacy_custom_schema_migrations(
        &pool,
        &state_migrator,
        LegacyMigrationCutover::Disabled,
    )
    .await
    .expect_err("legacy migration versions should require an exclusive cutover");
    let error = error.to_string();
    assert!(error.contains("stop all older Codex app-server and TUI processes"));
    assert!(error.contains("CODEX_STATE_LEGACY_MIGRATION_CUTOVER=1"));

    assert_eq!(
        sqlx::query_as::<_, (i64, String)>(
            "SELECT version, lower(hex(checksum)) FROM _sqlx_migrations \
             WHERE version IN (49, 50) ORDER BY version",
        )
        .fetch_all(&pool)
        .await
        .expect("legacy rows should remain unchanged"),
        vec![
            (49, CUSTOM_SCHEMA_V1.legacy_checksum.to_string()),
            (50, CUSTOM_SCHEMA_V2.legacy_checksum.to_string()),
        ]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
             AND name = 'codex_custom_schema_migrations'",
        )
        .fetch_one(&pool)
        .await
        .expect("custom registry absence should load"),
        0
    );

    migrate_legacy_custom_schema_migrations(
        &pool,
        &state_migrator,
        LegacyMigrationCutover::Enabled,
    )
    .await
    .expect("explicit cutover should adopt validated legacy migrations");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("official migrations should apply after explicit cutover");
    assert_eq!(
        sqlx::query_as::<_, (i64, Vec<u8>)>(
            "SELECT version, checksum FROM _sqlx_migrations \
             WHERE version IN (49, 50) ORDER BY version",
        )
        .fetch_all(&pool)
        .await
        .expect("official migration rows should load"),
        STATE_MIGRATOR
            .migrations
            .iter()
            .filter(|migration| matches!(migration.version, 49 | 50))
            .map(|migration| (migration.version, migration.checksum.to_vec()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        sqlx::query_as::<_, (i64, String, String)>(
            "SELECT version, name, definition FROM codex_custom_schema_migrations ORDER BY version",
        )
        .fetch_all(&pool)
        .await
        .expect("adopted custom rows should load"),
        vec![
            (
                1,
                CUSTOM_SCHEMA_V1.name.to_string(),
                CUSTOM_SCHEMA_V1.definition.to_string(),
            ),
            (
                2,
                CUSTOM_SCHEMA_V2.name.to_string(),
                CUSTOM_SCHEMA_V2.definition.to_string(),
            ),
        ]
    );
    assert_eq!(
        sqlx::query_as::<_, (String, i64)>(
            "SELECT thread_id, generation FROM thread_writer_generations",
        )
        .fetch_all(&pool)
        .await
        .expect("legacy writer generations should load"),
        vec![("00000000-0000-0000-0000-000000000049".to_string(), 7,)]
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT store_id FROM writer_authority_store WHERE singleton = 1",
        )
        .fetch_one(&pool)
        .await
        .expect("legacy store identity should load"),
        "legacy-store"
    );
    assert_eq!(
        sqlx::query_as::<_, (String, i64)>(
            "SELECT slot_id, generation FROM thread_execution_account_bindings",
        )
        .fetch_one(&pool)
        .await
        .expect("legacy execution binding should load"),
        ("account-a".to_string(), 3)
    );
    assert_eq!(
        sqlx::query_as::<_, (String, String, i64)>(
            "SELECT turn_id, slot_id, generation FROM thread_turn_execution_accounts",
        )
        .fetch_one(&pool)
        .await
        .expect("legacy turn provenance should load"),
        ("turn-1".to_string(), "account-a".to_string(), 3)
    );
    pool.close().await;
}

#[tokio::test]
async fn unknown_v50_fails_without_mutation() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("database should open");
    migrator_through(/*version*/ 48)
        .run(&pool)
        .await
        .expect("upstream migrations through 48 should apply");
    sqlx::raw_sql(CUSTOM_SCHEMA_V1.definition)
        .execute(&pool)
        .await
        .expect("legacy writer schema should apply");
    insert_legacy_custom_migration(&pool, &CUSTOM_SCHEMA_V1, CUSTOM_SCHEMA_V1.legacy_checksum)
        .await;
    insert_legacy_custom_migration(
        &pool,
        &CUSTOM_SCHEMA_V2,
        "05b8f0203829d22c122233012fa066fe6be269395e9e0f7a16848722f8ecd34124097826db94bef9925f88b381c2223b",
    )
    .await;
    let state_migrator = runtime_state_migrator();
    let error = migrate_legacy_custom_schema_migrations(
        &pool,
        &state_migrator,
        LegacyMigrationCutover::Enabled,
    )
    .await
    .expect_err("unknown v50 should fail before changing v49");
    assert!(error.to_string().contains("unknown checksum"));

    assert_eq!(
        sqlx::query_as::<_, (i64, String)>(
            "SELECT version, lower(hex(checksum)) FROM _sqlx_migrations \
             WHERE version IN (49, 50) ORDER BY version",
        )
        .fetch_all(&pool)
        .await
        .expect("legacy rows should remain unchanged"),
        vec![
            (49, CUSTOM_SCHEMA_V1.legacy_checksum.to_string()),
            (
                50,
                "05b8f0203829d22c122233012fa066fe6be269395e9e0f7a16848722f8ecd34124097826db94bef9925f88b381c2223b"
                    .to_string(),
            ),
        ]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
             AND name = 'codex_custom_schema_migrations'",
        )
        .fetch_one(&pool)
        .await
        .expect("custom registry absence should load"),
        0
    );
    pool.close().await;
}

#[tokio::test]
async fn thread_section_migration_preserves_legacy_pin_compatibility() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 44)
        .run(&pool)
        .await
        .expect("released thread migrations should apply");

    for thread_id in [
        "00000000-0000-0000-0000-000000000043",
        "00000000-0000-0000-0000-000000000044",
    ] {
        if thread_id.ends_with("44") {
            sqlx::query("UPDATE threads SET is_pinned = 1 WHERE id = ?")
                .bind("00000000-0000-0000-0000-000000000043")
                .execute(&pool)
                .await
                .expect("legacy pin should remain writable before section migration");
            STATE_MIGRATOR
                .run(&pool)
                .await
                .expect("section migration should apply");
        }
        sqlx::query(
            r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(thread_id)
        .bind("/tmp/legacy.jsonl")
        .bind(1_700_000_000_i64)
        .bind(1_700_000_000_i64)
        .bind(1_700_000_000_000_i64)
        .bind(1_700_000_000_000_i64)
        .bind("cli")
        .bind("openai")
        .bind("/tmp")
        .bind("")
        .bind("read-only")
        .bind("on-request")
        .execute(&pool)
        .await
        .expect("legacy thread insert should succeed");
    }

    let registered_sections = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT id, name, appearance FROM thread_sections ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("independent thread sections should load");
    assert_eq!(
        registered_sections,
        vec![(
            PINNED_THREAD_SECTION_ID.to_string(),
            PINNED_THREAD_SECTION_NAME.to_string(),
            None,
        )]
    );

    let threads = sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT is_pinned, thread_section_id FROM threads ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("legacy and section-aware thread metadata should load");
    assert_eq!(threads, vec![(1, None), (0, None)]);

    sqlx::query("INSERT INTO thread_sections (id, name) VALUES (?, ?)")
        .bind(CUSTOM_THREAD_SECTION_ID)
        .bind("Custom section")
        .execute(&pool)
        .await
        .expect("custom sections should have independent persisted identities");

    let thread_id = "00000000-0000-0000-0000-000000000043";
    sqlx::query("UPDATE threads SET thread_section_id = ? WHERE id = ?")
        .bind(CUSTOM_THREAD_SECTION_ID)
        .bind(thread_id)
        .execute(&pool)
        .await
        .expect("threads should reference independently persisted sections");
    sqlx::query("UPDATE threads SET is_pinned = 0 WHERE id = ?")
        .bind(thread_id)
        .execute(&pool)
        .await
        .expect("released binaries should still update the legacy pin column");
    let thread = sqlx::query_as::<_, (i64, Option<String>)>(
        "SELECT is_pinned, thread_section_id FROM threads WHERE id = ?",
    )
    .bind(thread_id)
    .fetch_one(&pool)
    .await
    .expect("legacy pin updates should not overwrite the authoritative section");
    assert_eq!(thread, (0, Some(CUSTOM_THREAD_SECTION_ID.to_string())));

    sqlx::query("UPDATE threads SET thread_section_id = NULL WHERE id = ?")
        .bind(thread_id)
        .execute(&pool)
        .await
        .expect("threads should be removable from sections");

    let registered_sections =
        sqlx::query_as::<_, (String, String)>("SELECT id, name FROM thread_sections ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("empty sections should remain independently discoverable");
    assert_eq!(
        registered_sections,
        vec![
            (
                CUSTOM_THREAD_SECTION_ID.to_string(),
                "Custom section".to_string(),
            ),
            (
                PINNED_THREAD_SECTION_ID.to_string(),
                PINNED_THREAD_SECTION_NAME.to_string(),
            ),
        ]
    );

    let mut released_pin_migrator = migrator_through(/*version*/ 44);
    released_pin_migrator.ignore_missing = true;
    released_pin_migrator
        .run(&pool)
        .await
        .expect("released pin-capable binaries should tolerate newer migrations");

    pool.close().await;
}

#[tokio::test]
async fn thread_section_order_migration_backfills_stably() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pool = sqlite
        .open_read_write_pool(&sqlite.state_db_path())
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 45)
        .run(&pool)
        .await
        .expect("pre-ordering migrations should apply");

    sqlx::query("INSERT INTO thread_sections (id, name) VALUES (?, ?)")
        .bind(CUSTOM_THREAD_SECTION_ID)
        .bind("Custom section")
        .execute(&pool)
        .await
        .expect("custom section should exist before threads reference it");

    let older = "00000000-0000-0000-0000-000000000071";
    let newer = "00000000-0000-0000-0000-000000000072";
    let pinned = "00000000-0000-0000-0000-000000000073";
    let unsectioned = "00000000-0000-0000-0000-000000000074";
    for (thread_id, recency_at_ms, section) in [
        (older, 1_700_000_001_000_i64, Some(CUSTOM_THREAD_SECTION_ID)),
        (newer, 1_700_000_002_000, Some(CUSTOM_THREAD_SECTION_ID)),
        (pinned, 1_700_000_003_000, Some(PINNED_THREAD_SECTION_ID)),
        (unsectioned, 1_700_000_004_000, None),
    ] {
        sqlx::query(
            r#"
INSERT INTO threads (
    id, rollout_path, created_at, updated_at, recency_at,
    created_at_ms, updated_at_ms, recency_at_ms, source,
    model_provider, cwd, title, preview, sandbox_policy, approval_mode, thread_section_id
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(thread_id)
        .bind("/tmp/legacy.jsonl")
        .bind(recency_at_ms / 1000)
        .bind(recency_at_ms / 1000)
        .bind(recency_at_ms / 1000)
        .bind(recency_at_ms)
        .bind(recency_at_ms)
        .bind(recency_at_ms)
        .bind("cli")
        .bind("openai")
        .bind("/tmp")
        .bind("")
        .bind("preview")
        .bind("read-only")
        .bind("on-request")
        .bind(section)
        .execute(&pool)
        .await
        .expect("legacy section row should insert");
    }

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("section ordering migration should apply");
    let custom_order = sqlx::query_scalar::<_, String>(
        "SELECT id FROM threads WHERE thread_section_id = ? ORDER BY section_position, id",
    )
    .bind(CUSTOM_THREAD_SECTION_ID)
    .fetch_all(&pool)
    .await
    .expect("backfilled custom order should load");
    assert_eq!(custom_order, vec![newer.to_string(), older.to_string()]);
    let positions =
        sqlx::query_scalar::<_, Option<i64>>("SELECT section_position FROM threads ORDER BY id")
            .fetch_all(&pool)
            .await
            .expect("section positions should load");
    assert_eq!(
        positions,
        vec![Some(2_000_000), Some(1_000_000), Some(1_000_000), None]
    );
    let entered = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT section_entered_at_ms FROM threads ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("section entry timestamps should load");
    assert_eq!(
        entered,
        vec![
            Some(1_700_000_001_000),
            Some(1_700_000_002_000),
            Some(1_700_000_003_000),
            None,
        ]
    );

    let section_position_index = sqlx::query_scalar::<_, String>(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND name = ?",
    )
    .bind("idx_threads_section_position")
    .fetch_optional(&pool)
    .await
    .expect("section position index should remain inspectable");
    assert_eq!(
        section_position_index,
        Some("idx_threads_section_position".to_string())
    );

    pool.close().await;
}

#[tokio::test]
async fn thread_item_update_ordinals_allow_older_writers() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let pre_update_ordinal_migrator = Migrator {
        migrations: Cow::Owned(
            THREAD_HISTORY_MIGRATOR
                .migrations
                .iter()
                .filter(|migration| migration.version < 4)
                .cloned()
                .collect(),
        ),
        ignore_missing: THREAD_HISTORY_MIGRATOR.ignore_missing,
        locking: THREAD_HISTORY_MIGRATOR.locking,
        table_name: THREAD_HISTORY_MIGRATOR.table_name.clone(),
        create_schemas: THREAD_HISTORY_MIGRATOR.create_schemas.clone(),
        no_tx: THREAD_HISTORY_MIGRATOR.no_tx,
    };
    let pool = sqlite
        .open_thread_history_db(
            &pre_update_ordinal_migrator,
            /*telemetry_override*/ None,
        )
        .await
        .expect("pre-update-ordinal migrations should apply");
    sqlx::query(
        r#"
INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_type, item_json) VALUES
    ('thread-1', 'turn-1', 'existing-item-1', 11, 1_100, 'userMessage', '{}'),
    ('thread-1', 'turn-1', 'existing-item-2', 12, 1_200, 'userMessage', '{}')
        "#,
    )
    .execute(&pool)
    .await
    .expect("pre-migration items should be inserted");
    THREAD_HISTORY_MIGRATOR
        .run(&pool)
        .await
        .expect("update-ordinal migration should apply");
    sqlx::query(
        r#"
INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_type, item_json) VALUES
    ('thread-1', 'turn-1', 'old-writer-item-1', 13, 1_300, 'userMessage', '{}'),
    ('thread-1', 'turn-1', 'old-writer-item-2', 14, 1_400, 'userMessage', '{}')
        "#,
    )
    .execute(&pool)
    .await
    .expect("older writers should be able to append multiple items after migration");
    let ordinals = sqlx::query_as::<_, (i64, i64)>(
        "SELECT rollout_ordinal, updated_at_ordinal FROM thread_items WHERE thread_id = ? ORDER BY rollout_ordinal",
    )
    .bind("thread-1")
    .fetch_all(&pool)
    .await
    .expect("old-writer items should load");
    assert_eq!(ordinals, vec![(11, 11), (12, 12), (13, 0), (14, 0)]);

    pool.close().await;
}

#[tokio::test]
async fn agent_job_tables_are_dropped_when_upgrading() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 15)
        .run(&pool)
        .await
        .expect("agent job migrations should apply");

    sqlx::query(
        r#"
INSERT INTO agent_jobs (
    id,
    name,
    status,
    instruction,
    input_headers_json,
    input_csv_path,
    output_csv_path,
    created_at,
    updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("job-1")
    .bind("legacy job")
    .bind("running")
    .bind("process rows")
    .bind(r#"["path"]"#)
    .bind("/tmp/input.csv")
    .bind("/tmp/output.csv")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_000_i64)
    .execute(&pool)
    .await
    .expect("legacy agent job should insert");
    sqlx::query(
        r#"
INSERT INTO agent_job_items (
    job_id,
    item_id,
    row_index,
    row_json,
    status,
    result_json,
    created_at,
    updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("job-1")
    .bind("item-1")
    .bind(0_i64)
    .bind(r#"{"path":"secret.csv"}"#)
    .bind("completed")
    .bind(r#"{"result":"legacy"}"#)
    .bind(1_700_000_000_i64)
    .bind(1_700_000_000_i64)
    .execute(&pool)
    .await
    .expect("legacy agent job item should insert");

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply");

    let agent_job_tables = sqlx::query_scalar::<_, String>(
        r#"
SELECT name
FROM sqlite_master
WHERE type = 'table' AND name IN ('agent_jobs', 'agent_job_items')
ORDER BY name
        "#,
    )
    .fetch_all(&pool)
    .await
    .expect("remaining agent job tables should load");
    assert_eq!(agent_job_tables, Vec::<String>::new());

    pool.close().await;
}

#[tokio::test]
async fn recency_migration_backfills_and_seeds_old_binary_inserts() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 37)
        .run(&pool)
        .await
        .expect("pre-recency migrations should apply");

    sqlx::query(
        r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000001")
    .bind("/tmp/first.jsonl")
    .bind(1_700_000_000_i64)
    .bind(1_700_000_100_i64)
    .bind(1_700_000_000_123_i64)
    .bind(1_700_000_100_456_i64)
    .bind("cli")
    .bind("openai")
    .bind("/tmp")
    .bind("")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("legacy row should insert");

    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("recency migration should apply");

    let backfilled = sqlx::query(
        "SELECT updated_at, updated_at_ms, recency_at, recency_at_ms FROM threads WHERE id = ?",
    )
    .bind("00000000-0000-0000-0000-000000000001")
    .fetch_one(&pool)
    .await
    .expect("backfilled row should load");
    assert_eq!(backfilled.get::<i64, _>("recency_at"), 1_700_000_100);
    assert_eq!(backfilled.get::<i64, _>("recency_at_ms"), 1_700_000_100_456);

    sqlx::query(
        r#"
INSERT INTO threads (
    id,
    rollout_path,
    created_at,
    updated_at,
    created_at_ms,
    updated_at_ms,
    source,
    model_provider,
    cwd,
    title,
    sandbox_policy,
    approval_mode
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind("00000000-0000-0000-0000-000000000002")
    .bind("/tmp/second.jsonl")
    .bind(1_700_000_200_i64)
    .bind(1_700_000_300_i64)
    .bind(1_700_000_200_123_i64)
    .bind(1_700_000_300_456_i64)
    .bind("cli")
    .bind("openai")
    .bind("/tmp")
    .bind("")
    .bind("read-only")
    .bind("on-request")
    .execute(&pool)
    .await
    .expect("old-binary row should insert");

    let seeded = sqlx::query("SELECT recency_at, recency_at_ms FROM threads WHERE id = ?")
        .bind("00000000-0000-0000-0000-000000000002")
        .fetch_one(&pool)
        .await
        .expect("old-binary row should load");
    assert_eq!(seeded.get::<i64, _>("recency_at"), 1_700_000_300);
    assert_eq!(seeded.get::<i64, _>("recency_at_ms"), 1_700_000_300_456);

    pool.close().await;
}

#[tokio::test]
async fn repairs_recency_migration_that_was_applied_as_version_38() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("sqlite database should open");
    migrator_through(/*version*/ 37)
        .run(&pool)
        .await
        .expect("pre-recency migrations should apply");

    let recency_migration = STATE_MIGRATOR
        .migrations
        .iter()
        .find(|migration| migration.version == 39)
        .expect("recency migration should exist");
    let mut legacy_migrations = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version <= 37)
        .cloned()
        .collect::<Vec<_>>();
    legacy_migrations.push(Migration::new(
        38,
        recency_migration.description.clone(),
        recency_migration.migration_type,
        recency_migration.sql.clone(),
        recency_migration.no_tx,
    ));
    let legacy_recency_migrator = Migrator::with_migrations(legacy_migrations);
    legacy_recency_migrator
        .run(&pool)
        .await
        .expect("legacy recency migration should apply as version 38");

    repair_legacy_recency_migration_version(&pool, &STATE_MIGRATOR)
        .await
        .expect("legacy migration history should be repaired");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply after repair");

    let applied = sqlx::query(
        "SELECT version, checksum FROM _sqlx_migrations WHERE version >= 38 ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    .expect("applied migrations should load")
    .into_iter()
    .map(|row| {
        (
            row.get::<i64, _>("version"),
            row.get::<Vec<u8>, _>("checksum"),
        )
    })
    .collect::<Vec<_>>();
    let expected = STATE_MIGRATOR
        .migrations
        .iter()
        .filter(|migration| migration.version >= 38)
        .map(|migration| (migration.version, migration.checksum.to_vec()))
        .collect::<Vec<_>>();
    assert_eq!(applied, expected);

    pool.close().await;
}

#[tokio::test]
async fn repair_recency_migration_succeeds_while_another_connection_holds_writer_slot() {
    let sqlite_home = crate::runtime::test_support::unique_temp_dir();
    tokio::fs::create_dir_all(&sqlite_home)
        .await
        .expect("sqlite home should be created");
    let _cleanup = scopeguard::guard(sqlite_home.clone(), |sqlite_home| {
        let _ = std::fs::remove_dir_all(sqlite_home);
    });
    let sqlite = crate::SqliteConfig::new_for_testing(sqlite_home.as_path().abs());
    let state_path = sqlite.state_db_path();
    let pool = sqlite
        .open_read_write_pool(&state_path)
        .await
        .expect("database should open");
    STATE_MIGRATOR
        .run(&pool)
        .await
        .expect("current migrations should apply");
    let read_pool = sqlite
        .open_read_only_pool(&state_path)
        .await
        .expect("read-only pool should open");
    let mut write_connection = pool.acquire().await.expect("write connection should open");
    let write_transaction = write_connection
        .begin_with("BEGIN IMMEDIATE")
        .await
        .expect("write transaction should acquire the writer slot");

    let repair_result = repair_legacy_recency_migration_version(&read_pool, &STATE_MIGRATOR).await;

    write_transaction
        .rollback()
        .await
        .expect("write transaction should roll back");
    drop(write_connection);
    read_pool.close().await;
    pool.close().await;
    repair_result.expect("current migration history should not need the writer slot");
}
