use std::borrow::Cow;

use sqlx::SqliteConnection;
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;

pub(crate) static STATE_MIGRATOR: Migrator = sqlx::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR: Migrator = sqlx::migrate!("./logs_migrations");
pub(crate) static GOALS_MIGRATOR: Migrator = sqlx::migrate!("./goals_migrations");
pub(crate) static MEMORIES_MIGRATOR: Migrator = sqlx::migrate!("./memory_migrations");
pub(crate) static QUEUE_MIGRATOR: Migrator = sqlx::migrate!("./queue_migrations");
pub(crate) static THREAD_HISTORY_MIGRATOR: Migrator = sqlx::migrate!("./thread_history_migrations");

/// Allow a Codex binary to ignore migration versions newer than its embedded
/// migration set.
///
/// Known versions are still validated by checksum. This therefore does not
/// make binaries compatible when they assign different migrations to the same
/// version number.
fn runtime_migrator(base: &'static Migrator) -> Migrator {
    Migrator {
        migrations: Cow::Borrowed(base.migrations.as_ref()),
        ignore_missing: true,
        locking: base.locking,
        no_tx: base.no_tx,
        table_name: base.table_name.clone(),
        create_schemas: base.create_schemas.clone(),
    }
}

pub(crate) fn runtime_state_migrator() -> Migrator {
    runtime_migrator(&STATE_MIGRATOR)
}

pub(crate) fn runtime_logs_migrator() -> Migrator {
    runtime_migrator(&LOGS_MIGRATOR)
}

struct CustomSchemaMigration {
    version: i64,
    name: &'static str,
    definition: &'static str,
    legacy_upstream_version: i64,
    legacy_description: &'static str,
    legacy_checksum: &'static str,
    required_tables: &'static [RequiredCustomTable],
}

struct RequiredCustomTable {
    name: &'static str,
    definition: &'static str,
}

const WRITER_AUTHORITY_STORE_DEFINITION: &str = r#"CREATE TABLE writer_authority_store (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    store_id TEXT NOT NULL UNIQUE
)"#;
const THREAD_WRITER_GENERATIONS_DEFINITION: &str = r#"CREATE TABLE thread_writer_generations (
    thread_id TEXT PRIMARY KEY,
    generation INTEGER NOT NULL CHECK (generation > 0)
) WITHOUT ROWID"#;
const THREAD_EXECUTION_ACCOUNT_BINDINGS_DEFINITION: &str = r#"CREATE TABLE thread_execution_account_bindings (
    thread_id TEXT PRIMARY KEY NOT NULL,
    slot_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 1)
)"#;
const THREAD_TURN_EXECUTION_ACCOUNTS_DEFINITION: &str = r#"CREATE TABLE thread_turn_execution_accounts (
    thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    slot_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 1),
    PRIMARY KEY (thread_id, turn_id)
)"#;
const ACCOUNT_SLOT_RUNTIME_VERSIONS_DEFINITION: &str = r#"CREATE TABLE account_slot_runtime_versions (
    slot_id TEXT PRIMARY KEY NOT NULL,
    runtime_version INTEGER NOT NULL CHECK (runtime_version >= 1)
) WITHOUT ROWID"#;
const THREAD_TRANSITIONS_DEFINITION: &str = r#"CREATE TABLE thread_transitions (
    revision INTEGER PRIMARY KEY AUTOINCREMENT,
    transition_id TEXT NOT NULL UNIQUE,
    request_fingerprint TEXT NOT NULL,
    reason TEXT NOT NULL CHECK(reason IN ('clear','new')),
    previous_thread_id TEXT NOT NULL,
    current_thread_id TEXT NOT NULL UNIQUE,
    origin_instance_epoch TEXT NOT NULL,
    initiator_client_incarnation TEXT NOT NULL,
    previous_precondition_state_revision INTEGER NOT NULL CHECK(previous_precondition_state_revision >= 1),
    previous_committed_state_revision INTEGER CHECK(previous_committed_state_revision >= 1),
    previous_writer_store_id TEXT NOT NULL,
    previous_writer_generation INTEGER NOT NULL CHECK(previous_writer_generation >= 1),
    current_writer_store_id TEXT,
    current_writer_generation INTEGER CHECK(current_writer_generation >= 1),
    current_committed_state_revision INTEGER CHECK(current_committed_state_revision >= 1),
    status TEXT NOT NULL CHECK(status IN ('preparing','prepared','committed')),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    committed_at INTEGER
)"#;
const THREAD_ACCOUNT_ROTATION_POLICIES_DEFINITION: &str = r#"CREATE TABLE thread_account_rotation_policies (
    thread_id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    mode TEXT NOT NULL CHECK (mode IN ('fixed','quotaAware','roundRobin','exhaustThenNext')),
    fixed_slot_id TEXT,
    automatic_slot_ids_json TEXT NOT NULL,
    last_committed_slot_id TEXT,
    updated_at INTEGER NOT NULL
)"#;

const CUSTOM_SCHEMA_V1: CustomSchemaMigration = CustomSchemaMigration {
    version: 1,
    name: "writer_authority",
    definition: r#"
CREATE TABLE writer_authority_store (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    store_id TEXT NOT NULL UNIQUE
);

CREATE TABLE thread_writer_generations (
    thread_id TEXT PRIMARY KEY,
    generation INTEGER NOT NULL CHECK (generation > 0)
) WITHOUT ROWID;
"#,
    legacy_upstream_version: 49,
    legacy_description: "writer authority",
    legacy_checksum: "1bc0543e5728e0df193ffcbbc214861790304b7cad234378cafdee0cefd02de9e7a94df439d8bb681d20d34e9ef89a40",
    required_tables: &[
        RequiredCustomTable {
            name: "writer_authority_store",
            definition: WRITER_AUTHORITY_STORE_DEFINITION,
        },
        RequiredCustomTable {
            name: "thread_writer_generations",
            definition: THREAD_WRITER_GENERATIONS_DEFINITION,
        },
    ],
};
const CUSTOM_SCHEMA_V2: CustomSchemaMigration = CustomSchemaMigration {
    version: 2,
    name: "execution_account_bindings",
    definition: r#"
CREATE TABLE thread_execution_account_bindings (
    thread_id TEXT PRIMARY KEY NOT NULL,
    slot_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 1)
);

CREATE TABLE thread_turn_execution_accounts (
    thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    slot_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 1),
    PRIMARY KEY (thread_id, turn_id)
);
"#,
    legacy_upstream_version: 50,
    legacy_description: "thread execution account bindings",
    legacy_checksum: "35b8f0203829d22c122233012fa066fe6be269395e9e0f7a16848722f8ecd34124097826db94bef9925f88b381c2223b",
    required_tables: &[
        RequiredCustomTable {
            name: "thread_execution_account_bindings",
            definition: THREAD_EXECUTION_ACCOUNT_BINDINGS_DEFINITION,
        },
        RequiredCustomTable {
            name: "thread_turn_execution_accounts",
            definition: THREAD_TURN_EXECUTION_ACCOUNTS_DEFINITION,
        },
    ],
};
const CUSTOM_SCHEMA_V3: CustomSchemaMigration = CustomSchemaMigration {
    version: 3,
    name: "account_slot_runtime_versions",
    definition: r#"
CREATE TABLE account_slot_runtime_versions (
    slot_id TEXT PRIMARY KEY NOT NULL,
    runtime_version INTEGER NOT NULL CHECK (runtime_version >= 1)
) WITHOUT ROWID;
"#,
    // V3 was never written into the upstream migration registry and is intentionally absent from
    // `LEGACY_CUSTOM_SCHEMA_MIGRATIONS`; these legacy-only fields are therefore unused sentinels.
    legacy_upstream_version: 0,
    legacy_description: "",
    legacy_checksum: "",
    required_tables: &[RequiredCustomTable {
        name: "account_slot_runtime_versions",
        definition: ACCOUNT_SLOT_RUNTIME_VERSIONS_DEFINITION,
    }],
};
const CUSTOM_SCHEMA_V4: CustomSchemaMigration = CustomSchemaMigration {
    version: 4,
    name: "thread_transitions",
    definition: r#"
CREATE TABLE thread_transitions (
    revision INTEGER PRIMARY KEY AUTOINCREMENT,
    transition_id TEXT NOT NULL UNIQUE,
    request_fingerprint TEXT NOT NULL,
    reason TEXT NOT NULL CHECK(reason IN ('clear','new')),
    previous_thread_id TEXT NOT NULL,
    current_thread_id TEXT NOT NULL UNIQUE,
    origin_instance_epoch TEXT NOT NULL,
    initiator_client_incarnation TEXT NOT NULL,
    previous_precondition_state_revision INTEGER NOT NULL CHECK(previous_precondition_state_revision >= 1),
    previous_committed_state_revision INTEGER CHECK(previous_committed_state_revision >= 1),
    previous_writer_store_id TEXT NOT NULL,
    previous_writer_generation INTEGER NOT NULL CHECK(previous_writer_generation >= 1),
    current_writer_store_id TEXT,
    current_writer_generation INTEGER CHECK(current_writer_generation >= 1),
    current_committed_state_revision INTEGER CHECK(current_committed_state_revision >= 1),
    status TEXT NOT NULL CHECK(status IN ('preparing','prepared','committed')),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    committed_at INTEGER
);
CREATE UNIQUE INDEX thread_transitions_one_committed_outgoing_authority
ON thread_transitions(
    previous_thread_id,
    previous_precondition_state_revision,
    previous_writer_store_id,
    previous_writer_generation
) WHERE status = 'committed';
CREATE INDEX thread_transitions_current_status
ON thread_transitions(current_thread_id, status);
"#,
    legacy_upstream_version: 0,
    legacy_description: "",
    legacy_checksum: "",
    required_tables: &[RequiredCustomTable {
        name: "thread_transitions",
        definition: THREAD_TRANSITIONS_DEFINITION,
    }],
};
const CUSTOM_SCHEMA_V5: CustomSchemaMigration = CustomSchemaMigration {
    version: 5,
    name: "thread_account_rotation_policies",
    definition: r#"
CREATE TABLE thread_account_rotation_policies (
    thread_id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    mode TEXT NOT NULL CHECK (mode IN ('fixed','quotaAware','roundRobin','exhaustThenNext')),
    fixed_slot_id TEXT,
    automatic_slot_ids_json TEXT NOT NULL,
    last_committed_slot_id TEXT,
    updated_at INTEGER NOT NULL
);
"#,
    legacy_upstream_version: 0,
    legacy_description: "",
    legacy_checksum: "",
    required_tables: &[RequiredCustomTable {
        name: "thread_account_rotation_policies",
        definition: THREAD_ACCOUNT_ROTATION_POLICIES_DEFINITION,
    }],
};

const ACTIVE_CUSTOM_SCHEMA_MIGRATIONS: &[&CustomSchemaMigration] = &[
    &CUSTOM_SCHEMA_V1,
    &CUSTOM_SCHEMA_V2,
    &CUSTOM_SCHEMA_V3,
    &CUSTOM_SCHEMA_V4,
    &CUSTOM_SCHEMA_V5,
];
const LEGACY_CUSTOM_SCHEMA_MIGRATIONS: &[&CustomSchemaMigration] =
    &[&CUSTOM_SCHEMA_V1, &CUSTOM_SCHEMA_V2];

pub(crate) const LEGACY_MIGRATION_CUTOVER_ENV: &str = "CODEX_STATE_LEGACY_MIGRATION_CUTOVER";

pub(crate) enum LegacyMigrationCutover {
    Disabled,
    Enabled,
}

pub(crate) async fn apply_custom_schema_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS codex_custom_schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    definition TEXT NOT NULL,
    applied_at INTEGER NOT NULL DEFAULT (unixepoch())
)
        "#,
    )
    .execute(pool)
    .await?;

    for migration in ACTIVE_CUSTOM_SCHEMA_MIGRATIONS {
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let applied = sqlx::query_as::<_, (String, String)>(
            "SELECT name, definition FROM codex_custom_schema_migrations WHERE version = ?",
        )
        .bind(migration.version)
        .fetch_optional(&mut *transaction)
        .await?;
        if let Some((name, definition)) = applied {
            if name != migration.name || definition != migration.definition {
                anyhow::bail!(
                    "custom schema migration {} does not match its stored definition",
                    migration.version
                );
            }
            transaction.commit().await?;
            continue;
        }

        sqlx::raw_sql(migration.definition)
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "INSERT INTO codex_custom_schema_migrations (version, name, definition) \
             VALUES (?, ?, ?)",
        )
        .bind(migration.version)
        .bind(migration.name)
        .bind(migration.definition)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
    }
    Ok(())
}

/// Reject legacy custom migrations unless this process was explicitly started
/// for a one-time cutover after all older writers were stopped.
pub(crate) async fn migrate_legacy_custom_schema_migrations(
    pool: &SqlitePool,
    official_migrator: &Migrator,
    cutover: LegacyMigrationCutover,
) -> anyhow::Result<()> {
    match cutover {
        LegacyMigrationCutover::Disabled => {
            let mut connection = pool.acquire().await?;
            let legacy_to_adopt =
                legacy_custom_schema_migrations_to_adopt(&mut connection, official_migrator)
                    .await?;
            if let Some(migration) = legacy_to_adopt.first() {
                anyhow::bail!(
                    "legacy custom state migration {} conflicts with the official migration; \
                     stop all older Codex app-server and TUI processes sharing this state store, \
                     then restart once with {LEGACY_MIGRATION_CUTOVER_ENV}=1",
                    migration.legacy_upstream_version
                );
            }
            return Ok(());
        }
        LegacyMigrationCutover::Enabled => {}
    }

    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let legacy_to_adopt =
        legacy_custom_schema_migrations_to_adopt(&mut transaction, official_migrator).await?;
    if legacy_to_adopt.is_empty() {
        transaction.commit().await?;
        return Ok(());
    }

    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS codex_custom_schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    definition TEXT NOT NULL,
    applied_at INTEGER NOT NULL DEFAULT (unixepoch())
)
        "#,
    )
    .execute(&mut *transaction)
    .await?;
    for migration in legacy_to_adopt {
        sqlx::query(
            "INSERT INTO codex_custom_schema_migrations (version, name, definition) \
             VALUES (?, ?, ?) ON CONFLICT(version) DO NOTHING",
        )
        .bind(migration.version)
        .bind(migration.name)
        .bind(migration.definition)
        .execute(&mut *transaction)
        .await?;
        let deleted = sqlx::query(
            "DELETE FROM _sqlx_migrations WHERE version = ? AND success = TRUE \
             AND description = ? AND lower(hex(checksum)) = ?",
        )
        .bind(migration.legacy_upstream_version)
        .bind(migration.legacy_description)
        .bind(migration.legacy_checksum)
        .execute(&mut *transaction)
        .await?;
        if deleted.rows_affected() != 1 {
            anyhow::bail!(
                "legacy custom migration {} changed during adoption",
                migration.legacy_upstream_version
            );
        }
    }
    transaction.commit().await?;
    Ok(())
}

async fn legacy_custom_schema_migrations_to_adopt(
    connection: &mut SqliteConnection,
    official_migrator: &Migrator,
) -> anyhow::Result<Vec<&'static CustomSchemaMigration>> {
    let upstream_registry_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(&mut *connection)
    .await?
    .is_some();
    if !upstream_registry_exists {
        return Ok(Vec::new());
    }

    let upstream_rows = sqlx::query_as::<_, (i64, String, bool, String)>(
        "SELECT version, description, success, lower(hex(checksum)) FROM _sqlx_migrations \
         WHERE version IN (49, 50) ORDER BY version",
    )
    .fetch_all(&mut *connection)
    .await?;
    let mut legacy_to_adopt = Vec::new();
    for migration in LEGACY_CUSTOM_SCHEMA_MIGRATIONS {
        let Some((_, description, success, checksum)) = upstream_rows
            .iter()
            .find(|(version, _, _, _)| *version == migration.legacy_upstream_version)
        else {
            continue;
        };
        if !success {
            anyhow::bail!(
                "state migration {} is not marked successful",
                migration.legacy_upstream_version
            );
        }
        let official = official_migrator
            .migrations
            .iter()
            .find(|official| official.version == migration.legacy_upstream_version)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "official state migration {} is unavailable",
                    migration.legacy_upstream_version
                )
            })?;
        let official_checksum = checksum_hex(official.checksum.as_ref());
        if checksum == &official_checksum && description == official.description.as_ref() {
            continue;
        }
        if checksum == migration.legacy_checksum && description == migration.legacy_description {
            legacy_to_adopt.push(*migration);
            continue;
        }
        anyhow::bail!(
            "state migration {} has an unknown checksum",
            migration.legacy_upstream_version
        );
    }

    for migration in &legacy_to_adopt {
        for table in migration.required_tables {
            let definition = sqlx::query_scalar::<_, String>(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?",
            )
            .bind(table.name)
            .fetch_optional(&mut *connection)
            .await?;
            if definition.as_deref() != Some(table.definition) {
                anyhow::bail!(
                    "legacy custom migration {} table {} is missing or does not match",
                    migration.legacy_upstream_version,
                    table.name
                );
            }
        }
    }

    let custom_registry_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' \
         AND name = 'codex_custom_schema_migrations'",
    )
    .fetch_optional(&mut *connection)
    .await?
    .is_some();
    if custom_registry_exists {
        let custom_rows = sqlx::query_as::<_, (i64, String, String)>(
            "SELECT version, name, definition FROM codex_custom_schema_migrations \
             WHERE version IN (1, 2)",
        )
        .fetch_all(&mut *connection)
        .await?;
        for migration in &legacy_to_adopt {
            let Some((_, name, definition)) = custom_rows
                .iter()
                .find(|(version, _, _)| *version == migration.version)
            else {
                continue;
            };
            if name != migration.name || definition != migration.definition {
                anyhow::bail!(
                    "custom schema migration {} does not match legacy adoption metadata",
                    migration.version
                );
            }
        }
    }

    Ok(legacy_to_adopt)
}

fn checksum_hex(checksum: &[u8]) -> String {
    use std::fmt::Write;

    checksum.iter().fold(
        String::with_capacity(checksum.len() * 2),
        |mut encoded, byte| {
            let _ = write!(encoded, "{byte:02x}");
            encoded
        },
    )
}

pub(crate) fn runtime_goals_migrator() -> Migrator {
    runtime_migrator(&GOALS_MIGRATOR)
}

pub(crate) fn runtime_memories_migrator() -> Migrator {
    runtime_migrator(&MEMORIES_MIGRATOR)
}

pub(crate) fn runtime_queue_migrator() -> Migrator {
    runtime_migrator(&QUEUE_MIGRATOR)
}

// The paginated history projector will call this when it takes ownership of opening the database.
#[allow(dead_code)]
pub(crate) fn runtime_thread_history_migrator() -> Migrator {
    runtime_migrator(&THREAD_HISTORY_MIGRATOR)
}

pub(crate) async fn repair_legacy_recency_migration_version(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let Some(recency_migration) = migrator
        .migrations
        .iter()
        .find(|migration| migration.version == 39)
    else {
        return Ok(());
    };
    let migrations_table_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?
    .is_some();
    if !migrations_table_exists {
        return Ok(());
    }

    let legacy_recency_needs_repair = sqlx::query_scalar::<_, i64>(
        r#"
SELECT 1
FROM _sqlx_migrations
WHERE version = ?
  AND checksum = ?
  AND NOT EXISTS (
      SELECT 1 FROM _sqlx_migrations WHERE version = ?
  )
        "#,
    )
    .bind(38_i64)
    .bind(recency_migration.checksum.as_ref())
    .bind(recency_migration.version)
    .fetch_optional(pool)
    .await?
    .is_some();
    if !legacy_recency_needs_repair {
        return Ok(());
    }

    sqlx::query(
        r#"
UPDATE _sqlx_migrations
SET version = ?, description = ?
WHERE version = ?
  AND checksum = ?
  AND NOT EXISTS (
      SELECT 1 FROM _sqlx_migrations WHERE version = ?
  )
        "#,
    )
    .bind(recency_migration.version)
    .bind(recency_migration.description.as_ref())
    .bind(38_i64)
    .bind(recency_migration.checksum.as_ref())
    .bind(recency_migration.version)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
