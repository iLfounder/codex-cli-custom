use std::path::PathBuf;
use std::sync::Arc;

use codex_protocol::ThreadId;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutConfig;
use codex_rollout::RolloutRecorder;
use codex_rollout::RolloutRecorderParams;
use tokio::sync::OwnedMutexGuard;

use super::LocalThreadStore;
use super::writer_lock::WriterLockGuard;
use crate::LoadThreadHistoryParams;
use crate::PrepareThreadResumeParams;
use crate::PrepareThreadResumeTarget;
use crate::PreparedThreadResume;
use crate::PreparedThreadResumeAuthority;
use crate::ReadThreadParams;
use crate::ThreadPersistenceMetadata;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;

struct LocalPreparedThreadResume {
    _live_writer_guard: OwnedMutexGuard<()>,
    writer_lock: WriterLockGuard,
    thread_id: ThreadId,
    rollout_path: PathBuf,
    history_mode: ThreadHistoryMode,
}

pub(super) async fn prepare(
    store: &LocalThreadStore,
    params: PrepareThreadResumeParams,
) -> ThreadStoreResult<PreparedThreadResume> {
    // A path resume intentionally ignores the request's thread ID. Reading the immutable session
    // identity is the only rollout access allowed before exact writer authority is known.
    let path_target = match &params.target {
        PrepareThreadResumeTarget::ThreadId(_) => None,
        PrepareThreadResumeTarget::RolloutPath(path) => {
            let path =
                super::read_thread::resolve_requested_rollout_path(store, path.clone()).await?;
            let session_meta = codex_rollout::read_session_meta_line(path.as_path())
                .await
                .map_err(|err| ThreadStoreError::Internal {
                    message: format!(
                        "failed to read session identity from {}: {err}",
                        path.display()
                    ),
                })?;
            Some((path, session_meta.meta.id))
        }
    };
    let thread_id = match params.target {
        PrepareThreadResumeTarget::ThreadId(thread_id) => thread_id,
        PrepareThreadResumeTarget::RolloutPath(_) => path_target
            .as_ref()
            .map(|(_, thread_id)| *thread_id)
            .ok_or_else(|| ThreadStoreError::Internal {
                message: "prepared path resume lost its thread identity".to_string(),
            })?,
    };

    let live_writer_guard = store.live_writer_locks.lock(thread_id).await;
    store.ensure_live_recorder_absent(thread_id).await?;
    let writer_lock = store.acquire_writer_lock(thread_id).await?;

    let mut stored_thread = match path_target {
        Some((path, _)) => {
            super::read_thread::read_thread_by_rollout_path(
                store,
                path,
                params.include_archived,
                /*include_history*/ false,
            )
            .await?
        }
        None => {
            super::read_thread::read_thread(
                store,
                ReadThreadParams {
                    thread_id,
                    include_archived: params.include_archived,
                    include_history: false,
                },
            )
            .await?
        }
    };
    if stored_thread.thread_id != thread_id {
        return Err(ThreadStoreError::InvalidRequest {
            message: format!(
                "prepared resume resolved thread {thread_id} to thread {}",
                stored_thread.thread_id
            ),
        });
    }
    let rollout_path =
        stored_thread
            .rollout_path
            .clone()
            .ok_or_else(|| ThreadStoreError::Internal {
                message: format!("thread {thread_id} does not have a rollout path"),
            })?;
    let initial_history = if stored_thread.history_mode == ThreadHistoryMode::Paginated {
        super::model_context::load_latest_model_context(
            store,
            LoadThreadHistoryParams {
                thread_id,
                include_archived: params.include_archived,
            },
        )
        .await?
        .items
    } else {
        super::read_thread::load_history_items(rollout_path.as_path()).await?
    };
    stored_thread.history = None;
    let authority = PreparedThreadResumeAuthority::new(
        thread_id,
        LocalPreparedThreadResume {
            _live_writer_guard: live_writer_guard,
            writer_lock,
            thread_id,
            rollout_path,
            history_mode: stored_thread.history_mode,
        },
    );
    Ok(PreparedThreadResume::new(
        stored_thread,
        Arc::new(initial_history),
        authority,
    ))
}

pub(super) async fn activate(
    store: &LocalThreadStore,
    authority: PreparedThreadResumeAuthority,
    metadata: ThreadPersistenceMetadata,
) -> ThreadStoreResult<()> {
    let prepared = authority.downcast::<LocalPreparedThreadResume>()?;
    let LocalPreparedThreadResume {
        _live_writer_guard,
        writer_lock,
        thread_id,
        rollout_path,
        history_mode,
    } = *prepared;
    let cwd = metadata
        .cwd
        .ok_or_else(|| ThreadStoreError::InvalidRequest {
            message: "local thread store requires a cwd".to_string(),
        })?;
    let config = RolloutConfig {
        codex_home: store.config.codex_home.clone(),
        sqlite: store.config.sqlite.clone(),
        cwd,
        model_provider_id: metadata.model_provider,
        generate_memories: matches!(metadata.memory_mode, ThreadMemoryMode::Enabled),
    };
    let rollout_id = super::thread_rollout_resolver::rollout_id_from_path_or_legacy_thread_id(
        rollout_path.as_path(),
        thread_id,
        history_mode,
    )?;
    let recorder = RolloutRecorder::new(&config, RolloutRecorderParams::resume(rollout_path))
        .await
        .map_err(|err| ThreadStoreError::Internal {
            message: format!("failed to resume local thread recorder: {err}"),
        })?;
    store
        .insert_live_recorder(thread_id, recorder, rollout_id, history_mode, writer_lock)
        .await
}
