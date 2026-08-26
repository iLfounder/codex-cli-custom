use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_protocol::protocol::ThreadHistoryMode;
use std::any::Any;
use std::future::Future;
use std::pin::Pin;

use crate::AbortThreadTransition;
use crate::AccountBindingCommitIntent;
use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::ArchiveThreadsParams;
use crate::CommitThreadTransition;
use crate::CommittedThreadTransitions;
use crate::CreateProjectParams;
use crate::CreateThreadParams;
use crate::CreateThreadSectionParams;
use crate::CreatedProject;
use crate::DeleteThreadParams;
use crate::DeleteThreadSectionParams;
use crate::DeleteThreadsParams;
use crate::DeletedProject;
use crate::ItemPage;
use crate::ListItemsParams;
use crate::ListProjectsParams;
use crate::ListThreadSectionsParams;
use crate::ListThreadsParams;
use crate::ListTurnsParams;
use crate::LoadThreadHistoryParams;
use crate::MarkThreadTransitionPrepared;
use crate::MoveProjectParams;
use crate::MoveThreadToSectionParams;
use crate::PrepareForkParams;
use crate::PreparedFork;
use crate::ProjectMoveOutcome;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::RenameThreadSectionParams;
use crate::ResumeThreadParams;
use crate::RevertThreadParams;
use crate::SearchThreadOccurrencesParams;
use crate::SearchThreadsParams;
use crate::StoredModelContext;
use crate::StoredProject;
use crate::StoredProjectsPage;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::StoredThreadSection;
use crate::StoredThreadSectionsPage;
use crate::ThreadAccountRotationPolicy;
use crate::ThreadAccountRotationPolicyUpdate;
use crate::ThreadMetadataPatch;
use crate::ThreadOccurrenceSearchPage;
use crate::ThreadPage;
use crate::ThreadSearchPage;
use crate::ThreadStoreError;
use crate::ThreadStoreResult;
use crate::ThreadTransitionAbortOutcome;
use crate::ThreadTransitionClaimOutcome;
use crate::ThreadTransitionCommitOutcome;
use crate::ThreadTransitionIntent;
use crate::ThreadTransitionPreparation;
use crate::ThreadTransitionRecord;
use crate::ThreadWriterEvidence;
use crate::TurnPage;
use crate::UpdateProjectParams;
use crate::UpdateThreadMetadataParams;
use crate::UpdatedProject;
use crate::WriterControlCapability;

/// Future returned by [`ThreadStore`] operations.
pub type ThreadStoreFuture<'a, T> = Pin<Box<dyn Future<Output = ThreadStoreResult<T>> + Send + 'a>>;

/// A slot runtime version and its complete durable binding set.
pub type ExecutionAccountSlotRuntimeState = (u64, Vec<(ThreadId, ExecutionAccountBinding)>);

/// Why thread persistence is being requested.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistContext {
    /// Standard persistence makes the thread and all queued items durable and readable.
    Standard,
    /// A turn is about to begin sampling after its input has been recorded.
    TurnStart,
}

/// Storage-neutral writer ownership exposed to runtime observers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeWriterOwnership {
    None,
    OwnedHere,
    OwnedElsewhere,
    Unavailable,
}

/// Storage-neutral durable position that never exposes a backing path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimePersistencePosition {
    pub ordinal: u64,
    pub offset: u64,
}

/// Health of one runtime persistence stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimePersistenceHealth {
    Unknown,
    Healthy,
    Degraded,
    Failed,
}

/// Sanitized store state used by app-server runtime snapshots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadStoreRuntimeSnapshot {
    pub writer_ownership: RuntimeWriterOwnership,
    pub writer_store_id: Option<String>,
    pub writer_generation: Option<u64>,
    pub writer_deny_reason: Option<String>,
    pub jsonl: Option<RuntimePersistencePosition>,
    pub sqlite: Option<RuntimePersistencePosition>,
    pub lag: Option<u64>,
    pub flush_health: RuntimePersistenceHealth,
    pub materialize_health: RuntimePersistenceHealth,
    pub flushed_at: Option<i64>,
    pub materialized_at: Option<i64>,
    pub persistence_deny_reason: Option<String>,
}

impl ThreadStoreRuntimeSnapshot {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            writer_ownership: RuntimeWriterOwnership::Unavailable,
            writer_store_id: None,
            writer_generation: None,
            writer_deny_reason: Some(reason.into()),
            jsonl: None,
            sqlite: None,
            lag: None,
            flush_health: RuntimePersistenceHealth::Unknown,
            materialize_health: RuntimePersistenceHealth::Unknown,
            flushed_at: None,
            materialized_at: None,
            persistence_deny_reason: None,
        }
    }
}

/// Storage-neutral thread persistence boundary.
pub trait ThreadStore: Any + Send + Sync {
    /// Return this store as [`Any`] for implementation-owned escape hatches.
    fn as_any(&self) -> &dyn Any;

    /// Returns the history mode to use when history does not carry a persisted mode.
    ///
    /// The default is legacy so existing stores stay compatible. Stores whose durable contract is
    /// already paginated should override this instead of relying on core to infer storage behavior.
    fn default_history_mode(&self) -> ThreadHistoryMode {
        ThreadHistoryMode::Legacy
    }

    /// Reports whether this process has persistent writer fencing for strict control operations.
    fn writer_control_capability(&self) -> WriterControlCapability {
        WriterControlCapability::Disabled {
            reason: "persistent writer control requires the state database".to_string(),
        }
    }

    /// Read sanitized writer and persistence state without scanning backing files.
    fn runtime_snapshot(
        &self,
        _thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, ThreadStoreRuntimeSnapshot> {
        Box::pin(async {
            Ok(ThreadStoreRuntimeSnapshot::unavailable(
                "thread store runtime state is unavailable",
            ))
        })
    }

    fn execution_account_binding(
        &self,
        _thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, Option<ExecutionAccountBinding>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "execution_account_binding",
            })
        })
    }

    fn initialize_execution_account_binding(
        &self,
        _thread_id: ThreadId,
        _initial: ExecutionAccountBinding,
    ) -> ThreadStoreFuture<'_, ExecutionAccountBinding> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "initialize_execution_account_binding",
            })
        })
    }

    fn compare_and_swap_execution_account_binding(
        &self,
        thread_id: ThreadId,
        expected: ExecutionAccountBinding,
        next_slot_id: String,
    ) -> ThreadStoreFuture<'_, Option<ExecutionAccountBinding>> {
        self.compare_and_swap_execution_account_binding_with_intent(
            thread_id,
            expected,
            next_slot_id,
            AccountBindingCommitIntent::PinFixed,
        )
    }

    fn compare_and_swap_execution_account_binding_with_intent(
        &self,
        _thread_id: ThreadId,
        _expected: ExecutionAccountBinding,
        _next_slot_id: String,
        _intent: AccountBindingCommitIntent,
    ) -> ThreadStoreFuture<'_, Option<ExecutionAccountBinding>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "compare_and_swap_execution_account_binding_with_intent",
            })
        })
    }

    fn thread_account_rotation_policy(
        &self,
        _thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, ThreadAccountRotationPolicy> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread_account_rotation_policy",
            })
        })
    }

    fn compare_and_swap_thread_account_rotation_policy(
        &self,
        _thread_id: ThreadId,
        _expected_revision: u64,
        _update: ThreadAccountRotationPolicyUpdate,
    ) -> ThreadStoreFuture<'_, Option<ThreadAccountRotationPolicy>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "compare_and_swap_thread_account_rotation_policy",
            })
        })
    }

    fn compare_and_swap_thread_account_rotation_cursor(
        &self,
        _thread_id: ThreadId,
        _expected_revision: u64,
        _accepted_account_slot_id: String,
    ) -> ThreadStoreFuture<'_, Option<ThreadAccountRotationPolicy>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "compare_and_swap_thread_account_rotation_cursor",
            })
        })
    }

    fn remove_account_slot_from_automatic_rotation_policies(
        &self,
        _account_slot_id: String,
    ) -> ThreadStoreFuture<'_, Vec<(ThreadId, ThreadAccountRotationPolicy)>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "remove_account_slot_from_automatic_rotation_policies",
            })
        })
    }

    /// Returns one consistent runtime version and the complete durable binding set for a slot.
    ///
    /// A slot without a runtime-version record has version zero.
    fn execution_account_slot_runtime_state(
        &self,
        _slot_id: String,
    ) -> ThreadStoreFuture<'_, ExecutionAccountSlotRuntimeState> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "execution_account_slot_runtime_state",
            })
        })
    }

    /// Atomically advances one slot runtime and every exact expected binding generation.
    ///
    /// Returns `None` without mutation if the runtime version or complete binding set is stale.
    fn compare_and_swap_execution_account_slot_runtime(
        &self,
        _slot_id: String,
        _expected_runtime_version: u64,
        _expected_bindings: Vec<(ThreadId, ExecutionAccountBinding)>,
    ) -> ThreadStoreFuture<'_, Option<ExecutionAccountSlotRuntimeState>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "compare_and_swap_execution_account_slot_runtime",
            })
        })
    }

    fn claim_thread_transition(
        &self,
        _intent: ThreadTransitionIntent,
        _reserved_current_thread_id: ThreadId,
        _origin_instance_epoch: String,
        _initiator_client_incarnation: String,
        _previous_writer: ThreadWriterEvidence,
    ) -> ThreadStoreFuture<'_, ThreadTransitionClaimOutcome> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "claim_thread_transition",
            })
        })
    }

    fn mark_thread_transition_prepared(
        &self,
        _request: MarkThreadTransitionPrepared,
    ) -> ThreadStoreFuture<'_, ThreadTransitionPreparation> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "mark_thread_transition_prepared",
            })
        })
    }

    fn abort_thread_transition(
        &self,
        _request: AbortThreadTransition,
    ) -> ThreadStoreFuture<'_, ThreadTransitionAbortOutcome> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "abort_thread_transition",
            })
        })
    }

    fn commit_thread_transition(
        &self,
        _request: CommitThreadTransition,
    ) -> ThreadStoreFuture<'_, ThreadTransitionCommitOutcome> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "commit_thread_transition",
            })
        })
    }

    fn thread_transition_by_id(
        &self,
        _transition_id: String,
    ) -> ThreadStoreFuture<'_, Option<ThreadTransitionRecord>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread_transition_by_id",
            })
        })
    }

    fn committed_thread_transitions_for_threads(
        &self,
        _thread_ids: Vec<ThreadId>,
    ) -> ThreadStoreFuture<'_, std::collections::HashMap<ThreadId, CommittedThreadTransitions>>
    {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "committed_thread_transitions_for_threads",
            })
        })
    }

    fn turn_execution_account(
        &self,
        _thread_id: ThreadId,
        _turn_id: String,
    ) -> ThreadStoreFuture<'_, Option<ExecutionAccountBinding>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "turn_execution_account",
            })
        })
    }

    /// Creates a new live thread.
    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Stages host-owned metadata for a thread ID reserved before Core starts the thread.
    ///
    /// The entry remains in memory until the first successful metadata update for that thread.
    /// Callers must remove it if startup fails before the store opens a live thread.
    fn stage_pending_thread_metadata(
        &self,
        _thread_id: ThreadId,
        _patch: ThreadMetadataPatch,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "stage_pending_thread_metadata",
            })
        })
    }

    /// Removes host-owned metadata staged for a reserved thread ID.
    fn remove_pending_thread_metadata(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "remove_pending_thread_metadata",
            })
        })
    }

    /// Reopens an existing thread for live appends.
    fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Appends raw rollout items to a live thread.
    ///
    /// Implementations should apply the shared rollout persistence policy before writing durable
    /// replay history and before updating any implementation-owned projections.
    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()>;

    /// Materializes the thread if persistence is lazy, then persists all queued items.
    ///
    /// Standard persistence must complete before returning. Turn-start persistence may complete
    /// in the background when the implementation enqueues it before returning, fences it with
    /// subsequent flush or shutdown operations, and surfaces failures through those operations.
    fn persist_thread(
        &self,
        thread_id: ThreadId,
        context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()>;

    /// Flushes all queued items and returns once they are durable/readable.
    fn flush_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()>;

    /// Flushes pending items and closes the live thread writer.
    fn shutdown_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()>;

    /// Strictly durabilizes and releases the live writer for an explicit ownership handoff.
    ///
    /// Implementations must validate `expected_writer_generation` before mutating persistence and
    /// retain the writer when any durability step fails. Only success may release ownership.
    fn relinquish_thread(
        &self,
        thread_id: ThreadId,
        expected_writer_generation: u64,
    ) -> ThreadStoreFuture<'_, ()>;

    /// Validates the exact live writer generation without mutating persistence.
    fn validate_writer_generation(
        &self,
        _thread_id: ThreadId,
        _expected_writer_generation: u64,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "validate_writer_generation",
            })
        })
    }

    /// Discards the live thread writer without forcing pending in-memory items to become durable.
    ///
    /// Core calls this when session initialization fails after a live writer has been created.
    /// Implementations should release any live writer resources for the thread while preserving
    /// already-durable thread data.
    fn discard_thread(&self, thread_id: ThreadId) -> ThreadStoreFuture<'_, ()>;

    /// Loads persisted history for resume, fork, rollback, and memory jobs.
    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory>;

    /// Loads the persisted rollout items needed to reconstruct the latest model-visible context.
    ///
    /// Implementations that cannot perform a targeted read may return the full persisted history.
    fn load_latest_model_context(
        &self,
        _params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredModelContext> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "load_latest_model_context",
            })
        })
    }

    /// Freezes source history and model context used to initialize a referenced fork.
    ///
    /// Stores without reference-backed fork support can retain this default implementation.
    fn prepare_fork(&self, _params: PrepareForkParams) -> ThreadStoreFuture<'_, PreparedFork> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "prepare_fork",
            })
        })
    }

    /// Reverts a paginated thread's durable history so it ends immediately before
    /// `before_turn_id`.
    ///
    /// Callers must close the thread's live writer first. The logical thread id and semantic
    /// metadata stay unchanged.
    ///
    /// Stores without paginated revert support can retain this default implementation.
    fn revert_thread(&self, _params: RevertThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "revert_thread",
            })
        })
    }

    /// Reads a thread summary and optionally its persisted history.
    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread>;

    /// Reads a rollout-backed thread by path when the store supports path-addressed lookups.
    ///
    /// Deprecated: new callers should use [`ThreadStore::read_thread`] instead.
    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread>;

    /// Lists stored threads matching the supplied filters.
    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage>;

    /// Whether this store can discover and manage independently persisted thread sections.
    fn supports_thread_sections(&self) -> bool {
        false
    }

    /// Lists independently persisted thread sections.
    fn list_thread_sections(
        &self,
        _params: ListThreadSectionsParams,
    ) -> ThreadStoreFuture<'_, StoredThreadSectionsPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/list",
            })
        })
    }

    /// Creates a custom thread section with a stable, server-assigned identity.
    fn create_thread_section(
        &self,
        _params: CreateThreadSectionParams,
    ) -> ThreadStoreFuture<'_, StoredThreadSection> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/create",
            })
        })
    }

    /// Renames a custom thread section, returning `None` when it does not exist.
    fn rename_thread_section(
        &self,
        _params: RenameThreadSectionParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThreadSection>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/update",
            })
        })
    }

    /// Deletes a custom thread section and reports whether it existed.
    fn delete_thread_section(
        &self,
        _params: DeleteThreadSectionParams,
    ) -> ThreadStoreFuture<'_, bool> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "threadSection/delete",
            })
        })
    }

    /// Whether this store supports durable host-owned projects.
    fn supports_projects(&self) -> bool {
        false
    }

    fn list_projects(
        &self,
        _params: ListProjectsParams,
    ) -> ThreadStoreFuture<'_, StoredProjectsPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/list",
            })
        })
    }

    fn read_project(&self, _project_id: String) -> ThreadStoreFuture<'_, Option<StoredProject>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/read",
            })
        })
    }

    fn create_project(
        &self,
        _params: CreateProjectParams,
    ) -> ThreadStoreFuture<'_, CreatedProject> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/create",
            })
        })
    }

    fn update_project(
        &self,
        _params: UpdateProjectParams,
    ) -> ThreadStoreFuture<'_, Option<UpdatedProject>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/update",
            })
        })
    }

    fn move_project(
        &self,
        _params: MoveProjectParams,
    ) -> ThreadStoreFuture<'_, Option<ProjectMoveOutcome>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/move",
            })
        })
    }

    fn delete_project(&self, _project_id: String) -> ThreadStoreFuture<'_, Option<DeletedProject>> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "project/delete",
            })
        })
    }

    /// Whether paginated threads can hydrate durable history through turn and item lists.
    fn supports_paginated_history_lists(&self) -> bool {
        false
    }

    /// Searches stored threads and returns search-only preview metadata.
    fn search_threads(
        &self,
        _params: SearchThreadsParams,
    ) -> ThreadStoreFuture<'_, ThreadSearchPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/search",
            })
        })
    }

    /// Searches visible message occurrences within one paginated thread.
    fn search_thread_occurrences(
        &self,
        _params: SearchThreadOccurrencesParams,
    ) -> ThreadStoreFuture<'_, ThreadOccurrenceSearchPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/searchOccurrences",
            })
        })
    }

    /// Lists turns within a stored thread.
    fn list_turns(&self, _params: ListTurnsParams) -> ThreadStoreFuture<'_, TurnPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "list_turns",
            })
        })
    }

    /// Lists persisted items within a stored thread, optionally filtered to a turn.
    fn list_items(&self, _params: ListItemsParams) -> ThreadStoreFuture<'_, ItemPage> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "list_items",
            })
        })
    }

    /// Applies a literal metadata patch and returns the updated thread when one was materialized.
    ///
    /// `None` means the update succeeded without materializing a thread, for example because the
    /// implementation filtered the patch to a no-op. Callers that require a `StoredThread` must
    /// perform a fallback read.
    ///
    /// Implementations should apply the supplied fields directly. Policy such as deciding whether
    /// an append-derived preview should be emitted belongs above the store.
    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThread>>;

    /// Moves a thread to, within, or out of a server-ordered section.
    fn move_thread_to_section(
        &self,
        _params: MoveThreadToSectionParams,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async {
            Err(ThreadStoreError::Unsupported {
                operation: "thread/section/move",
            })
        })
    }

    /// Archives a thread.
    fn archive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Archives threads in order, returning the successfully archived thread ids.
    ///
    /// The first thread must archive successfully; later failures are best effort.
    fn archive_threads(
        &self,
        params: ArchiveThreadsParams,
    ) -> ThreadStoreFuture<'_, Vec<ThreadId>> {
        Box::pin(async move {
            let mut archived_thread_ids = Vec::new();
            for thread_id in params.thread_ids {
                match self.archive_thread(ArchiveThreadParams { thread_id }).await {
                    Ok(()) => archived_thread_ids.push(thread_id),
                    Err(err) if archived_thread_ids.is_empty() => return Err(err),
                    Err(err) => tracing::warn!("failed to archive thread {thread_id}: {err}"),
                }
            }
            Ok(archived_thread_ids)
        })
    }

    /// Unarchives a thread and returns its updated metadata.
    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread>;

    /// Deletes a thread's persisted rollout data and associated metadata.
    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()>;

    /// Deletes threads in order, treating already-missing members as deleted.
    ///
    /// Stores with request-scoped delete preflight should override this instead of repeating
    /// that work through [`ThreadStore::delete_thread`].
    fn delete_threads(&self, params: DeleteThreadsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            for thread_id in params.thread_ids {
                match self.delete_thread(DeleteThreadParams { thread_id }).await {
                    Ok(()) | Err(ThreadStoreError::ThreadNotFound { .. }) => {}
                    Err(err) => return Err(err),
                }
            }
            Ok(())
        })
    }
}
