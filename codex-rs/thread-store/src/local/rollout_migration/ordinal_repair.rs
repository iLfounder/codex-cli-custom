use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::RolloutLine;
use codex_rollout::decode_rollout_line;
use codex_state::SqliteConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::fs::File;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::BufWriter;

use super::LocalThreadStore;
use super::MAX_ROLLOUT_LINE_BYTES;
use super::RolloutMigrationOutcome;
use super::RolloutMigrationReport;
use super::RolloutMigrationStatus;
use super::find_rollout_paths;
use super::migration_error;
use super::migration_journal_path;
use super::publish::ordinal_repair_backup_path;
use super::publish::ordinal_repair_projection_path;
use super::publish::ordinal_repair_staged_path;
use super::publish::remove_file_if_present;
use super::publish::sync_parent_directory;
use super::publish::write_migration_journal;
use super::thread_history;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::local::ThreadWriterOwnership;

#[derive(Clone, Debug, Eq, PartialEq)]
struct RolloutFileIdentity {
    len: u64,
    modified_secs: u64,
    modified_nanos: u32,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl RolloutFileIdentity {
    async fn read(path: &Path) -> ThreadStoreResult<Self> {
        let metadata = tokio::fs::metadata(path).await.map_err(migration_error)?;
        let modified = metadata
            .modified()
            .map_err(migration_error)?
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(migration_error)?;
        Ok(Self {
            len: metadata.len(),
            modified_secs: modified.as_secs(),
            modified_nanos: modified.subsec_nanos(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }

    fn token(&self) -> String {
        #[cfg(unix)]
        {
            format!(
                "v1:{}:{}:{}:{}:{}",
                self.len, self.modified_secs, self.modified_nanos, self.device, self.inode
            )
        }
        #[cfg(not(unix))]
        {
            format!(
                "v1:{}:{}:{}",
                self.len, self.modified_secs, self.modified_nanos
            )
        }
    }
}

struct OrdinalInspection {
    history_mode: ThreadHistoryMode,
    file_identity: RolloutFileIdentity,
    first_invalid_ordinal: Option<u64>,
    expected_ordinal: Option<u64>,
    first_invalid_record_index: Option<u64>,
    record_count: u64,
}

impl OrdinalInspection {
    fn affected_suffix_records(&self) -> Option<u64> {
        self.first_invalid_record_index
            .map(|index| self.record_count.saturating_sub(index))
    }

    fn outcome(
        &self,
        thread_id: ThreadId,
        rollout_path: PathBuf,
        status: RolloutMigrationStatus,
        mutation_count: u64,
        backup_path: Option<PathBuf>,
    ) -> RolloutMigrationOutcome {
        RolloutMigrationOutcome {
            thread_id: Some(thread_id),
            rollout_path,
            status,
            bytes_processed: self.file_identity.len,
            message: None,
            history_mode: Some(self.history_mode),
            file_identity: Some(self.file_identity.token()),
            first_invalid_ordinal: self.first_invalid_ordinal,
            expected_ordinal: self.expected_ordinal,
            affected_suffix_records: self.affected_suffix_records(),
            mutation_count: Some(mutation_count),
            backup_path,
        }
    }
}

impl LocalThreadStore {
    /// Inspect one exact paginated rollout, or repair its ordinal suffix after an identity-bound
    /// dry run. Passing an expected identity performs the apply operation.
    pub async fn repair_rollout_ordinals(
        &self,
        thread_id: ThreadId,
        expected_file_identity: Option<&str>,
    ) -> ThreadStoreResult<RolloutMigrationReport> {
        let rollout_path = self.find_exact_rollout_path(thread_id).await?;
        let inspection = inspect_rollout(thread_id, &rollout_path).await?;
        let Some(expected_file_identity) = expected_file_identity else {
            let status = if inspection.first_invalid_record_index.is_some() {
                RolloutMigrationStatus::Eligible
            } else {
                RolloutMigrationStatus::AlreadyPaginated
            };
            return Ok(RolloutMigrationReport {
                outcomes: vec![inspection.outcome(
                    thread_id,
                    rollout_path,
                    status,
                    /*mutation_count*/ 0,
                    /*backup_path*/ None,
                )],
            });
        };
        if inspection.file_identity.token() != expected_file_identity {
            return Err(ThreadStoreError::Conflict {
                message: format!(
                    "rollout identity changed; expected {expected_file_identity}, found {}",
                    inspection.file_identity.token()
                ),
            });
        }
        if self.state_db.is_none() {
            return Err(ThreadStoreError::Unsupported {
                operation: "repair_rollout_ordinals",
            });
        }
        let Some(first_invalid_record_index) = inspection.first_invalid_record_index else {
            return Ok(RolloutMigrationReport {
                outcomes: vec![inspection.outcome(
                    thread_id,
                    rollout_path,
                    RolloutMigrationStatus::AlreadyPaginated,
                    /*mutation_count*/ 0,
                    /*backup_path*/ None,
                )],
            });
        };

        let _maintenance_guard =
            codex_rollout::try_acquire_rollout_maintenance_lock(&self.config.codex_home)
                .map_err(migration_error)?
                .ok_or_else(|| ThreadStoreError::Conflict {
                    message: "rollout compression or another migration is already running"
                        .to_string(),
                })?;
        let _live_writer_guard = self.live_writer_locks.lock(thread_id).await;
        let authority = self.probe_writer_authority(thread_id).await?;
        if authority.ownership != ThreadWriterOwnership::None {
            return Err(ThreadStoreError::Conflict {
                message: format!("thread {thread_id} is loaded or has an active writer"),
            });
        }
        let _writer_guard = self.acquire_writer_lock(thread_id).await?;
        ensure_file_identity(&rollout_path, &inspection.file_identity).await?;

        let staged_path = ordinal_repair_staged_path(&rollout_path)?;
        let projection_path = ordinal_repair_projection_path(&rollout_path, thread_id)?;
        let journal_path = migration_journal_path(&self.config.codex_home, thread_id);
        if tokio::fs::try_exists(&journal_path)
            .await
            .map_err(migration_error)?
        {
            return Err(ThreadStoreError::Conflict {
                message: format!(
                    "thread {thread_id} has a pending rollout migration; recover it before ordinal repair"
                ),
            });
        }
        remove_file_if_present(&staged_path).await?;
        stage_repaired_rollout(
            &rollout_path,
            &staged_path,
            first_invalid_record_index,
            inspection
                .expected_ordinal
                .ok_or_else(|| migration_error("ordinal repair has no expected suffix ordinal"))?,
        )
        .await?;
        if let Err(error) = self
            .validate_isolated_projection(thread_id, &staged_path, &projection_path)
            .await
        {
            remove_file_if_present(&staged_path).await?;
            return Err(error);
        }
        if let Err(error) = ensure_file_identity(&rollout_path, &inspection.file_identity).await {
            remove_file_if_present(&staged_path).await?;
            return Err(error);
        }

        let backup_path =
            ordinal_repair_backup_path(&self.config.codex_home, thread_id, &rollout_path)?;
        create_private_backup(&rollout_path, &backup_path).await?;
        write_migration_journal(&journal_path).await?;
        if let Err(error) = ensure_file_identity(&rollout_path, &inspection.file_identity).await {
            remove_file_if_present(&journal_path).await?;
            remove_file_if_present(&staged_path).await?;
            remove_file_if_present(&backup_path).await?;
            return Err(error);
        }

        if let Err(error) = tokio::fs::rename(&staged_path, &rollout_path).await {
            remove_file_if_present(&journal_path).await?;
            remove_file_if_present(&staged_path).await?;
            return Err(migration_error(error));
        }
        sync_parent_directory(&rollout_path).await?;
        thread_history::delete_thread(self, thread_id).await?;
        super::super::thread_history_materialization::materialize_to_sqlite(
            self,
            thread_id,
            &rollout_path,
        )
        .await?;
        verify_projection(self, thread_id, &rollout_path).await?;
        remove_file_if_present(&journal_path).await?;
        sync_parent_directory(&journal_path).await?;

        Ok(RolloutMigrationReport {
            outcomes: vec![inspection.outcome(
                thread_id,
                rollout_path,
                RolloutMigrationStatus::Migrated,
                inspection.affected_suffix_records().unwrap_or(0),
                Some(backup_path),
            )],
        })
    }

    async fn find_exact_rollout_path(&self, thread_id: ThreadId) -> ThreadStoreResult<PathBuf> {
        let mut matches = Vec::new();
        for root in [
            self.config.codex_home.join(codex_rollout::SESSIONS_SUBDIR),
            self.config
                .codex_home
                .join(codex_rollout::ARCHIVED_SESSIONS_SUBDIR),
        ] {
            matches.extend(
                find_rollout_paths(&root)
                    .await?
                    .into_iter()
                    .filter(|path| super::thread_id_from_rollout_filename(path) == Some(thread_id)),
            );
        }
        match matches.as_slice() {
            [path] => Ok(path.clone()),
            [] => Err(ThreadStoreError::InvalidRequest {
                message: format!("no rollout found for thread {thread_id}"),
            }),
            _ => Err(ThreadStoreError::Conflict {
                message: format!("multiple rollouts found for thread {thread_id}"),
            }),
        }
    }

    async fn validate_isolated_projection(
        &self,
        thread_id: ThreadId,
        staged_path: &Path,
        projection_path: &Path,
    ) -> ThreadStoreResult<()> {
        tokio::fs::create_dir(projection_path)
            .await
            .map_err(migration_error)?;
        #[cfg(unix)]
        tokio::fs::set_permissions(projection_path, std::fs::Permissions::from_mode(0o700))
            .await
            .map_err(migration_error)?;
        let absolute_path =
            AbsolutePathBuf::try_from(projection_path.to_path_buf()).map_err(migration_error)?;
        let mut config = self.config.clone();
        config.sqlite = SqliteConfig::from_sqlite_home(absolute_path);
        let isolated = LocalThreadStore::new(config, self.state_db.clone());
        let result = async {
            super::super::thread_history_materialization::materialize_to_sqlite(
                &isolated,
                thread_id,
                staged_path,
            )
            .await?;
            verify_projection(&isolated, thread_id, staged_path).await
        }
        .await;
        if let Some(pool) = isolated.thread_history_db.get() {
            pool.close().await;
        }
        tokio::fs::remove_dir_all(projection_path)
            .await
            .map_err(migration_error)?;
        result
    }
}

async fn inspect_rollout(
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<OrdinalInspection> {
    if super::rollout_path_is_compressed(rollout_path) {
        return Err(ThreadStoreError::Unsupported {
            operation: "repair_compressed_rollout_ordinals",
        });
    }
    let file_identity = RolloutFileIdentity::read(rollout_path).await?;
    let file = File::open(rollout_path).await.map_err(migration_error)?;
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    let mut record_count = 0_u64;
    let mut expected = None;
    let mut history_mode = None;
    let mut first_invalid_ordinal = None;
    let mut expected_ordinal = None;
    let mut first_invalid_record_index = None;
    while read_strict_line(&mut reader, &mut bytes).await? {
        let line = decode_rollout_bytes(&bytes)?;
        if record_count == 0 {
            let RolloutItem::SessionMeta(session_meta) = &line.item else {
                return Err(migration_error("rollout head is not session metadata"));
            };
            if session_meta.meta.id != thread_id {
                return Err(migration_error(
                    "rollout metadata thread id does not match target",
                ));
            }
            history_mode = Some(session_meta.meta.history_mode);
            if session_meta.meta.history_mode != ThreadHistoryMode::Paginated {
                return Err(migration_error("ordinal repair requires paginated history"));
            }
            expected = Some(
                session_meta
                    .meta
                    .history_base
                    .map_or(0, |base| base.end_ordinal_exclusive),
            );
        }
        let current_expected = expected
            .ok_or_else(|| migration_error("rollout contains records before session metadata"))?;
        if first_invalid_record_index.is_none() && line.ordinal != Some(current_expected) {
            first_invalid_ordinal = line.ordinal;
            expected_ordinal = Some(current_expected);
            first_invalid_record_index = Some(record_count);
        }
        expected = current_expected.checked_add(1);
        if expected.is_none() {
            return Err(migration_error("rollout ordinal overflow"));
        }
        record_count = record_count
            .checked_add(1)
            .ok_or_else(|| migration_error("rollout record count overflow"))?;
    }
    if record_count == 0 {
        return Err(migration_error("rollout contains no records"));
    }
    Ok(OrdinalInspection {
        history_mode: history_mode
            .ok_or_else(|| migration_error("non-empty rollout has no history mode"))?,
        file_identity,
        first_invalid_ordinal,
        expected_ordinal,
        first_invalid_record_index,
        record_count,
    })
}

async fn stage_repaired_rollout(
    rollout_path: &Path,
    staged_path: &Path,
    first_invalid_record_index: u64,
    mut next_ordinal: u64,
) -> ThreadStoreResult<()> {
    let source_permissions = tokio::fs::metadata(rollout_path)
        .await
        .map_err(migration_error)?
        .permissions();
    let source = File::open(rollout_path).await.map_err(migration_error)?;
    let staged = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(staged_path)
        .await
        .map_err(migration_error)?;
    staged
        .set_permissions(source_permissions)
        .await
        .map_err(migration_error)?;
    let mut source = BufReader::new(source);
    let mut staged = BufWriter::new(staged);
    let mut bytes = Vec::new();
    let mut record_index = 0_u64;
    while read_strict_line(&mut source, &mut bytes).await? {
        if record_index < first_invalid_record_index {
            staged.write_all(&bytes).await.map_err(migration_error)?;
        } else {
            let mut line = decode_rollout_bytes(&bytes)?;
            line.ordinal = Some(next_ordinal);
            next_ordinal = next_ordinal
                .checked_add(1)
                .ok_or_else(|| migration_error("rollout ordinal overflow"))?;
            staged
                .write_all(
                    serde_json::to_string(&line)
                        .map_err(migration_error)?
                        .as_bytes(),
                )
                .await
                .map_err(migration_error)?;
            staged.write_all(b"\n").await.map_err(migration_error)?;
        }
        record_index = record_index
            .checked_add(1)
            .ok_or_else(|| migration_error("rollout record count overflow"))?;
    }
    staged.flush().await.map_err(migration_error)?;
    staged.get_ref().sync_all().await.map_err(migration_error)
}

async fn create_private_backup(source_path: &Path, backup_path: &Path) -> ThreadStoreResult<()> {
    let parent = backup_path
        .parent()
        .ok_or_else(|| migration_error("ordinal repair backup has no parent directory"))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(migration_error)?;
    #[cfg(unix)]
    tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        .await
        .map_err(migration_error)?;
    let mut source = File::open(source_path).await.map_err(migration_error)?;
    let backup = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(backup_path)
        .await
        .map_err(migration_error)?;
    #[cfg(unix)]
    backup
        .set_permissions(std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(migration_error)?;
    let mut backup = BufWriter::new(backup);
    tokio::io::copy(&mut source, &mut backup)
        .await
        .map_err(migration_error)?;
    backup.flush().await.map_err(migration_error)?;
    backup.get_ref().sync_all().await.map_err(migration_error)?;
    sync_parent_directory(backup_path).await
}

async fn verify_projection(
    store: &LocalThreadStore,
    thread_id: ThreadId,
    rollout_path: &Path,
) -> ThreadStoreResult<()> {
    let expected_length = tokio::fs::metadata(rollout_path)
        .await
        .map_err(migration_error)?
        .len();
    let file = File::open(rollout_path).await.map_err(migration_error)?;
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    let mut final_ordinal = None;
    while read_strict_line(&mut reader, &mut bytes).await? {
        final_ordinal = decode_rollout_bytes(&bytes)?.ordinal;
    }
    let expected_ordinal = final_ordinal
        .and_then(|ordinal| ordinal.checked_add(1))
        .ok_or_else(|| migration_error("repaired rollout has no next ordinal"))?;
    let projection = thread_history::projection_state(store, thread_id)
        .await?
        .ok_or_else(|| migration_error("repaired rollout has no SQLite projection"))?;
    if projection.next_byte_offset != expected_length || projection.next_ordinal != expected_ordinal
    {
        return Err(migration_error(
            "SQLite projection does not cover the complete repaired rollout",
        ));
    }
    Ok(())
}

fn decode_rollout_bytes(bytes: &[u8]) -> ThreadStoreResult<RolloutLine> {
    let value = serde_json::from_slice(bytes).map_err(migration_error)?;
    decode_rollout_line(value).map_err(migration_error)
}

async fn ensure_file_identity(
    path: &Path,
    expected: &RolloutFileIdentity,
) -> ThreadStoreResult<()> {
    let actual = RolloutFileIdentity::read(path).await?;
    if &actual == expected {
        Ok(())
    } else {
        Err(ThreadStoreError::Conflict {
            message: format!(
                "rollout changed after inspection; expected {}, found {}",
                expected.token(),
                actual.token()
            ),
        })
    }
}

async fn read_strict_line(
    reader: &mut BufReader<File>,
    bytes: &mut Vec<u8>,
) -> ThreadStoreResult<bool> {
    bytes.clear();
    let byte_count = reader
        .take((MAX_ROLLOUT_LINE_BYTES + 1) as u64)
        .read_until(b'\n', bytes)
        .await
        .map_err(migration_error)?;
    if byte_count == 0 {
        return Ok(false);
    }
    if byte_count > MAX_ROLLOUT_LINE_BYTES {
        return Err(migration_error("rollout record exceeds repair size limit"));
    }
    if bytes.last() != Some(&b'\n') {
        return Err(migration_error("rollout ends with an incomplete record"));
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(migration_error("rollout contains a blank record"));
    }
    Ok(true)
}
