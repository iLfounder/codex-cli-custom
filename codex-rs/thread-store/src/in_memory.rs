use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use chrono::DateTime;
use chrono::Utc;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_protocol::protocol::SessionContextWindow;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_rollout::RolloutItem;
use codex_rollout::persisted_rollout_items;

use crate::AbortThreadTransition;
use crate::AppendThreadItemsParams;
use crate::ArchiveThreadParams;
use crate::CommitThreadTransition;
use crate::CommittedThreadTransitions;
use crate::CreateThreadParams;
use crate::DeleteThreadParams;
use crate::ListThreadsParams;
use crate::LoadThreadHistoryParams;
use crate::MarkThreadTransitionPrepared;
use crate::MoveThreadToSectionParams;
use crate::PersistContext;
use crate::ReadThreadByRolloutPathParams;
use crate::ReadThreadParams;
use crate::ResumeThreadParams;
use crate::RuntimePersistenceHealth;
use crate::RuntimeWriterOwnership;
use crate::StoredModelContext;
use crate::StoredThread;
use crate::StoredThreadHistory;
use crate::ThreadMetadataPatch;
use crate::ThreadPage;
use crate::ThreadRelationFilter;
use crate::ThreadSortKey;
use crate::ThreadStore;
use crate::ThreadStoreError;
use crate::ThreadStoreFuture;
use crate::ThreadStoreResult;
use crate::ThreadStoreRuntimeSnapshot;
use crate::ThreadTransitionAbortOutcome;
use crate::ThreadTransitionClaimOutcome;
use crate::ThreadTransitionCommitOutcome;
use crate::ThreadTransitionIntent;
use crate::ThreadTransitionPreparation;
use crate::ThreadTransitionRecord;
use crate::ThreadWriterEvidence;
use crate::UpdateThreadMetadataParams;
use crate::error::reject_paginated_history_mode;

static IN_MEMORY_THREAD_STORES: OnceLock<Mutex<HashMap<String, Arc<InMemoryThreadStore>>>> =
    OnceLock::new();

mod thread_transition;

fn stores() -> &'static Mutex<HashMap<String, Arc<InMemoryThreadStore>>> {
    IN_MEMORY_THREAD_STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ItemSortKey;
    use crate::ListItemsParams;
    use crate::ListTurnsParams;
    use crate::SortDirection;
    use crate::StoredTurnItemsView;
    use crate::ThreadPersistenceMetadata;
    use crate::ThreadSortKey;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::protocol::SessionSource;

    #[tokio::test]
    async fn execution_account_binding_uses_exact_cas_semantics() {
        let store = InMemoryThreadStore::default();
        let thread_id = ThreadId::default();
        let initial = ExecutionAccountBinding {
            slot_id: "default".to_string(),
            generation: 1,
        };

        assert_eq!(
            store
                .initialize_execution_account_binding(thread_id, initial.clone())
                .await
                .expect("initialize binding"),
            initial
        );
        assert_eq!(
            store
                .compare_and_swap_execution_account_binding(
                    thread_id,
                    initial.clone(),
                    "secondary".to_string(),
                )
                .await
                .expect("compare and swap"),
            Some(ExecutionAccountBinding {
                slot_id: "secondary".to_string(),
                generation: 2,
            })
        );
        assert_eq!(
            store
                .compare_and_swap_execution_account_binding(
                    thread_id,
                    initial,
                    "stale".to_string(),
                )
                .await
                .expect("stale compare and swap"),
            None
        );
    }

    #[tokio::test]
    async fn execution_account_slot_runtime_batch_cas_is_atomic() {
        let store = InMemoryThreadStore::default();
        let first_thread =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("thread id");
        let second_thread =
            ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("thread id");
        let phantom_thread =
            ThreadId::from_string("00000000-0000-0000-0000-000000000003").expect("thread id");
        for (thread_id, generation) in [(first_thread, 1), (second_thread, 3)] {
            store
                .initialize_execution_account_binding(
                    thread_id,
                    ExecutionAccountBinding {
                        slot_id: "secondary".to_string(),
                        generation,
                    },
                )
                .await
                .expect("initialize binding");
        }

        let legacy = store
            .execution_account_slot_runtime_state("secondary".to_string())
            .await
            .expect("read legacy slot runtime");
        assert_eq!(legacy.0, 0);
        let committed = store
            .compare_and_swap_execution_account_slot_runtime(
                "secondary".to_string(),
                legacy.0,
                legacy.1.clone(),
            )
            .await
            .expect("commit slot runtime")
            .expect("exact snapshot should commit");
        assert_eq!(committed.0, 1);
        assert_eq!(
            committed
                .1
                .iter()
                .map(|(_, binding)| binding.generation)
                .collect::<Vec<_>>(),
            vec![2, 4]
        );

        store
            .initialize_execution_account_binding(
                phantom_thread,
                ExecutionAccountBinding {
                    slot_id: "secondary".to_string(),
                    generation: 1,
                },
            )
            .await
            .expect("insert phantom binding");
        let before_stale = store
            .execution_account_slot_runtime_state("secondary".to_string())
            .await
            .expect("read state with phantom");
        assert_eq!(
            store
                .compare_and_swap_execution_account_slot_runtime(
                    "secondary".to_string(),
                    committed.0,
                    committed.1,
                )
                .await
                .expect("phantom commit should not fail"),
            None
        );
        assert_eq!(
            store
                .execution_account_slot_runtime_state("secondary".to_string())
                .await
                .expect("read state after phantom rejection"),
            before_stale
        );

        {
            let mut state = store.state.lock().await;
            state
                .account_slot_runtime_versions
                .insert("secondary".to_string(), u64::MAX);
        }
        let overflow_state = store
            .execution_account_slot_runtime_state("secondary".to_string())
            .await
            .expect("read overflow state");
        store
            .compare_and_swap_execution_account_slot_runtime(
                "secondary".to_string(),
                overflow_state.0,
                overflow_state.1.clone(),
            )
            .await
            .expect_err("runtime version overflow should reject the whole batch");
        assert_eq!(
            store
                .execution_account_slot_runtime_state("secondary".to_string())
                .await
                .expect("read state after overflow"),
            overflow_state
        );
    }

    #[tokio::test]
    async fn default_turn_pagination_methods_return_unsupported() {
        let store = InMemoryThreadStore::default();
        let thread_id = ThreadId::default();

        let turns_err = store
            .list_turns(ListTurnsParams {
                thread_id,
                include_archived: true,
                cursor: None,
                page_size: 10,
                sort_direction: SortDirection::Asc,
                items_view: StoredTurnItemsView::Summary,
            })
            .await
            .expect_err("default list_turns should be unsupported");
        assert!(matches!(
            turns_err,
            ThreadStoreError::Unsupported {
                operation: "list_turns"
            }
        ));

        let items_err = store
            .list_items(ListItemsParams {
                thread_id,
                turn_id: None,
                include_archived: true,
                cursor: None,
                page_size: 10,
                sort_direction: SortDirection::Asc,
                sort_key: ItemSortKey::CreatedAtOrdinal,
                after_updated_at_ordinal: None,
            })
            .await
            .expect_err("default list_items should be unsupported");
        assert!(matches!(
            items_err,
            ThreadStoreError::Unsupported {
                operation: "list_items"
            }
        ));
    }

    #[tokio::test]
    async fn list_threads_filters_by_spawn_relationship() {
        let store = InMemoryThreadStore::default();
        let parent_thread_id = ThreadId::default();
        let child_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("valid thread id");
        let unrelated_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("valid thread id");
        let grandchild_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000003").expect("valid thread id");

        for (thread_id, parent_thread_id) in [
            (child_thread_id, Some(parent_thread_id)),
            (unrelated_thread_id, None),
            (grandchild_thread_id, Some(child_thread_id)),
        ] {
            store
                .create_thread(CreateThreadParams {
                    session_id: thread_id.into(),
                    thread_id,
                    extra_config: None,
                    forked_from_id: None,
                    parent_thread_id,
                    source: SessionSource::Exec,
                    thread_source: None,
                    originator: "test_originator".to_string(),
                    base_instructions: BaseInstructions::default(),
                    dynamic_tools: Vec::new(),
                    selected_capability_roots: Vec::new(),
                    multi_agent_version: None,
                    history_mode: ThreadHistoryMode::Legacy,
                    history_base: None,
                    subagent_history_start_ordinal: None,
                    initial_window_id: uuid::Uuid::now_v7().to_string(),
                    metadata: ThreadPersistenceMetadata {
                        cwd: None,
                        model_provider: "test-provider".to_string(),
                        memory_mode: ThreadMemoryMode::Enabled,
                    },
                })
                .await
                .expect("create thread");
        }

        store
            .move_thread_to_section(MoveThreadToSectionParams {
                thread_id: grandchild_thread_id,
                section: Some(codex_state::PINNED_THREAD_SECTION_ID.to_string()),
                before_thread_id: None,
            })
            .await
            .expect("pin grandchild thread");

        let page = ThreadStore::list_threads(
            &store,
            ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                section: None,
                project_id: None,
                archived: false,
                search_term: None,
                relation_filter: Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id)),
                use_state_db_only: false,
            },
        )
        .await
        .expect("list child threads");

        assert_eq!(
            page.items
                .into_iter()
                .map(|item| item.thread_id)
                .collect::<Vec<_>>(),
            vec![child_thread_id]
        );

        let page = ThreadStore::list_threads(
            &store,
            ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                section: None,
                project_id: None,
                archived: false,
                search_term: None,
                relation_filter: Some(ThreadRelationFilter::DescendantsOf(parent_thread_id)),
                use_state_db_only: false,
            },
        )
        .await
        .expect("list descendant threads");

        assert_eq!(
            page.items
                .into_iter()
                .map(|item| item.thread_id)
                .collect::<HashSet<_>>(),
            HashSet::from([child_thread_id, grandchild_thread_id])
        );

        let page = ThreadStore::list_threads(
            &store,
            ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                section: Some(Some(codex_state::PINNED_THREAD_SECTION_ID.to_string())),
                project_id: None,
                archived: false,
                search_term: None,
                relation_filter: Some(ThreadRelationFilter::DescendantsOf(parent_thread_id)),
                use_state_db_only: false,
            },
        )
        .await
        .expect("list pinned descendant threads");

        assert_eq!(
            page.items
                .into_iter()
                .map(|item| item.thread_id)
                .collect::<Vec<_>>(),
            vec![grandchild_thread_id]
        );

        let page = ThreadStore::list_threads(
            &store,
            ListThreadsParams {
                page_size: 10,
                cursor: None,
                sort_key: ThreadSortKey::CreatedAt,
                sort_direction: SortDirection::Desc,
                allowed_sources: Vec::new(),
                model_providers: None,
                cwd_filters: None,
                section: Some(None),
                project_id: None,
                archived: false,
                search_term: None,
                relation_filter: Some(ThreadRelationFilter::DescendantsOf(parent_thread_id)),
                use_state_db_only: false,
            },
        )
        .await
        .expect("list unsectioned descendant threads");

        assert_eq!(
            page.items
                .into_iter()
                .map(|item| item.thread_id)
                .collect::<Vec<_>>(),
            vec![child_thread_id]
        );
    }

    #[tokio::test]
    async fn paginated_threads_allow_metadata_reads_and_resume_but_reject_legacy_history_paths() {
        let store = InMemoryThreadStore::default();
        let thread_id = ThreadId::default();
        let rollout_path = PathBuf::from("/tmp/paginated-thread.jsonl");

        store
            .create_thread(create_thread_params(thread_id, ThreadHistoryMode::Legacy))
            .await
            .expect("create legacy thread");
        store
            .resume_thread(ResumeThreadParams {
                thread_id,
                rollout_path: Some(rollout_path.clone()),
                history: None,
                include_archived: false,
                metadata: thread_metadata(),
            })
            .await
            .expect("register rollout path");
        {
            let mut state = store.state.lock().await;
            state
                .created_threads
                .get_mut(&thread_id)
                .expect("created thread")
                .history_mode = ThreadHistoryMode::Paginated;
            let Some(RolloutItem::SessionMeta(meta_line)) = state
                .histories
                .get_mut(&thread_id)
                .and_then(|history| history.first_mut())
            else {
                panic!("canonical session meta");
            };
            meta_line.meta.history_mode = ThreadHistoryMode::Paginated;
        }

        let thread = store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: false,
                include_history: false,
            })
            .await
            .expect("metadata read");
        assert_eq!(thread.history_mode, ThreadHistoryMode::Paginated);
        assert!(thread.history.is_none());

        let thread = store
            .read_thread_by_rollout_path(ReadThreadByRolloutPathParams {
                rollout_path,
                include_archived: false,
                include_history: false,
            })
            .await
            .expect("metadata path read");
        assert_eq!(thread.history_mode, ThreadHistoryMode::Paginated);
        assert!(thread.history.is_none());

        assert_paginated_threads_unsupported(
            store
                .read_thread(ReadThreadParams {
                    thread_id,
                    include_archived: false,
                    include_history: true,
                })
                .await
                .expect_err("full history read should fail"),
        );
        assert_paginated_threads_unsupported(
            store
                .read_thread_by_rollout_path(ReadThreadByRolloutPathParams {
                    rollout_path: PathBuf::from("/tmp/paginated-thread.jsonl"),
                    include_archived: false,
                    include_history: true,
                })
                .await
                .expect_err("full history path read should fail"),
        );
        assert_paginated_threads_unsupported(
            store
                .load_history(LoadThreadHistoryParams {
                    thread_id,
                    include_archived: false,
                })
                .await
                .expect_err("history load should fail"),
        );
        store
            .resume_thread(ResumeThreadParams {
                thread_id,
                rollout_path: None,
                history: None,
                include_archived: false,
                metadata: thread_metadata(),
            })
            .await
            .expect("resume should succeed");
        assert_paginated_threads_unsupported(
            store
                .create_thread(create_thread_params(
                    ThreadId::default(),
                    ThreadHistoryMode::Paginated,
                ))
                .await
                .expect_err("paginated create should fail"),
        );
    }

    #[tokio::test]
    async fn metadata_update_returns_the_materialized_thread() {
        let store = InMemoryThreadStore::default();
        let thread_id = ThreadId::default();
        store
            .create_thread(create_thread_params(thread_id, ThreadHistoryMode::Legacy))
            .await
            .expect("create thread");

        let updated = ThreadStore::update_thread_metadata(
            &store,
            UpdateThreadMetadataParams {
                thread_id,
                patch: ThreadMetadataPatch {
                    name: Some(Some("renamed".to_string())),
                    ..Default::default()
                },
                include_archived: false,
            },
        )
        .await
        .expect("update metadata");
        let updated = updated.expect("in-memory store returns updated thread");
        assert_eq!(updated.thread_id, thread_id);
        assert_eq!(updated.name.as_deref(), Some("renamed"));
    }

    fn create_thread_params(
        thread_id: ThreadId,
        history_mode: ThreadHistoryMode,
    ) -> CreateThreadParams {
        CreateThreadParams {
            session_id: thread_id.into(),
            thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "test_originator".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode,
            history_base: None,
            subagent_history_start_ordinal: None,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            metadata: thread_metadata(),
        }
    }

    fn thread_metadata() -> ThreadPersistenceMetadata {
        ThreadPersistenceMetadata {
            cwd: None,
            model_provider: "test-provider".to_string(),
            memory_mode: ThreadMemoryMode::Enabled,
        }
    }

    fn assert_paginated_threads_unsupported(err: ThreadStoreError) {
        assert!(matches!(
            err,
            ThreadStoreError::Unsupported {
                operation: "paginated_threads"
            }
        ));
    }
}

#[cfg(test)]
#[path = "in_memory_thread_transition_tests.rs"]
mod thread_transition_tests;

fn stores_guard() -> MutexGuard<'static, HashMap<String, Arc<InMemoryThreadStore>>> {
    match stores().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Recorded call counts for [`InMemoryThreadStore`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InMemoryThreadStoreCalls {
    pub create_thread: usize,
    pub resume_thread: usize,
    pub append_items: usize,
    pub persist_thread: usize,
    pub flush_thread: usize,
    pub shutdown_thread: usize,
    pub discard_thread: usize,
    pub load_history: usize,
    pub load_latest_model_context: usize,
    pub read_thread: usize,
    pub read_thread_with_history: usize,
    pub read_thread_by_rollout_path: usize,
    pub list_threads: usize,
    pub update_thread_metadata: usize,
    pub archive_thread: usize,
    pub unarchive_thread: usize,
    pub delete_thread: usize,
}

/// In-memory [`ThreadStore`] implementation for tests and debug configs.
///
/// Test and debug configs can select this store by id, letting tests exercise
/// config-driven non-local persistence without requiring the real remote gRPC
/// service.
#[derive(Default)]
pub struct InMemoryThreadStore {
    state: tokio::sync::Mutex<InMemoryThreadStoreState>,
    omit_metadata_update_result: AtomicBool,
}

#[derive(Default)]
struct InMemoryThreadStoreState {
    calls: InMemoryThreadStoreCalls,
    created_threads: HashMap<ThreadId, CreateThreadParams>,
    histories: HashMap<ThreadId, Vec<RolloutItem>>,
    metadata_updates: HashMap<ThreadId, ThreadMetadataPatch>,
    sections: HashMap<ThreadId, String>,
    section_positions: HashMap<ThreadId, i64>,
    section_entered_at: HashMap<ThreadId, DateTime<Utc>>,
    names: HashMap<ThreadId, Option<String>>,
    rollout_paths: HashMap<PathBuf, ThreadId>,
    execution_accounts: HashMap<ThreadId, ExecutionAccountBinding>,
    account_slot_runtime_versions: HashMap<String, u64>,
    turn_execution_accounts: HashMap<(ThreadId, String), ExecutionAccountBinding>,
    thread_transitions: HashMap<String, InMemoryThreadTransition>,
    next_thread_transition_revision: u64,
}

#[derive(Clone)]
struct InMemoryThreadTransition {
    revision: u64,
    request_fingerprint: String,
    previous_precondition_state_revision: u64,
    record: ThreadTransitionRecord,
}

impl InMemoryThreadStore {
    /// Returns the store associated with `id`, creating it if needed.
    pub fn for_id(id: impl Into<String>) -> Arc<Self> {
        let id = id.into();
        let mut stores = stores_guard();
        stores
            .entry(id)
            .or_insert_with(|| Arc::new(Self::default()))
            .clone()
    }

    /// Removes a shared in-memory store for `id`.
    pub fn remove_id(id: &str) -> Option<Arc<Self>> {
        stores_guard().remove(id)
    }

    /// Returns the calls observed by this store.
    pub async fn calls(&self) -> InMemoryThreadStoreCalls {
        self.state.lock().await.calls.clone()
    }

    /// Makes metadata updates apply normally while returning no materialized thread.
    pub fn omit_metadata_update_result_for_testing(&self) {
        self.omit_metadata_update_result
            .store(true, Ordering::Relaxed);
    }

    async fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreResult<()> {
        reject_paginated_history_mode(params.history_mode)?;
        let mut state = self.state.lock().await;
        state.calls.create_thread += 1;
        let session_meta = SessionMeta {
            session_id: params.session_id,
            id: params.thread_id,
            forked_from_id: params.forked_from_id,
            parent_thread_id: params.parent_thread_id,
            cwd: params.metadata.cwd.clone().unwrap_or_default(),
            agent_nickname: params.source.get_nickname(),
            agent_role: params.source.get_agent_role(),
            agent_path: params.source.get_agent_path().map(Into::into),
            originator: params.originator.clone(),
            source: params.source.clone(),
            thread_source: params.thread_source.clone(),
            model_provider: Some(params.metadata.model_provider.clone()),
            base_instructions: Some(params.base_instructions.clone()),
            dynamic_tools: (!params.dynamic_tools.is_empty()).then(|| params.dynamic_tools.clone()),
            selected_capability_roots: params.selected_capability_roots.clone(),
            memory_mode: matches!(params.metadata.memory_mode, ThreadMemoryMode::Disabled)
                .then_some("disabled".to_string()),
            history_mode: params.history_mode,
            history_base: params.history_base,
            subagent_history_start_ordinal: params.subagent_history_start_ordinal,
            multi_agent_version: params.multi_agent_version,
            context_window: Some(SessionContextWindow::new(params.initial_window_id.clone())),
            ..SessionMeta::default()
        };
        state
            .histories
            .entry(params.thread_id)
            .or_default()
            .push(RolloutItem::SessionMeta(SessionMetaLine {
                meta: session_meta,
                git: None,
            }));
        state.created_threads.insert(params.thread_id, params);
        Ok(())
    }

    async fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreResult<()> {
        let mut state = self.state.lock().await;
        state.calls.resume_thread += 1;
        if let Some(history) = params.history {
            state
                .histories
                .insert(params.thread_id, Arc::unwrap_or_clone(history));
        } else {
            state.histories.entry(params.thread_id).or_default();
        }
        if let Some(rollout_path) = params.rollout_path {
            state.rollout_paths.insert(rollout_path, params.thread_id);
        }
        Ok(())
    }

    async fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreResult<()> {
        if params.items.is_empty() {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        let history_mode = history_mode_from_state(&state, params.thread_id);
        let persisted_items = persisted_rollout_items(params.items.as_slice(), history_mode);
        if persisted_items.is_empty() {
            return Ok(());
        }
        state.calls.append_items += 1;
        state
            .histories
            .entry(params.thread_id)
            .or_default()
            .extend(persisted_items.clone());
        for item in persisted_items {
            if let RolloutItem::TurnContext(context) = item
                && let (Some(turn_id), Some(binding)) = (context.turn_id, context.execution_account)
            {
                let key = (params.thread_id, turn_id);
                match state.turn_execution_accounts.get(&key) {
                    Some(existing) if existing != &binding => {
                        return Err(ThreadStoreError::Conflict {
                            message: "turn execution account provenance is immutable".to_string(),
                        });
                    }
                    Some(_) => {}
                    None => {
                        state.turn_execution_accounts.insert(key, binding);
                    }
                }
            }
        }
        Ok(())
    }

    async fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreResult<StoredThreadHistory> {
        let mut state = self.state.lock().await;
        state.calls.load_history += 1;
        let items =
            state
                .histories
                .get(&params.thread_id)
                .ok_or(ThreadStoreError::ThreadNotFound {
                    thread_id: params.thread_id,
                })?;
        let history_mode = history_mode_from_state(&state, params.thread_id);
        reject_paginated_history_mode(history_mode)?;
        Ok(StoredThreadHistory {
            thread_id: params.thread_id,
            items: items.clone(),
        })
    }

    async fn load_latest_model_context(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreResult<StoredModelContext> {
        let mut state = self.state.lock().await;
        state.calls.load_latest_model_context += 1;
        let items =
            state
                .histories
                .get(&params.thread_id)
                .ok_or(ThreadStoreError::ThreadNotFound {
                    thread_id: params.thread_id,
                })?;
        Ok(StoredModelContext {
            thread_id: params.thread_id,
            items: items.clone(),
        })
    }

    async fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreResult<StoredThread> {
        let mut state = self.state.lock().await;
        state.calls.read_thread += 1;
        if params.include_history {
            state.calls.read_thread_with_history += 1;
            reject_paginated_history_mode(history_mode_from_state(&state, params.thread_id))?;
        }
        let thread = stored_thread_from_state(&state, params.thread_id, params.include_history)?;
        Ok(thread)
    }

    async fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreResult<StoredThread> {
        let mut state = self.state.lock().await;
        state.calls.read_thread_by_rollout_path += 1;
        let Some(thread_id) = state.rollout_paths.get(&params.rollout_path).copied() else {
            return Err(ThreadStoreError::InvalidRequest {
                message: format!(
                    "in-memory thread store does not know rollout path {}",
                    params.rollout_path.display()
                ),
            });
        };
        if params.include_history {
            reject_paginated_history_mode(history_mode_from_state(&state, thread_id))?;
        }
        let thread = stored_thread_from_state(&state, thread_id, params.include_history)?;
        Ok(thread)
    }

    async fn list_threads(&self) -> ThreadStoreResult<ThreadPage> {
        let mut state = self.state.lock().await;
        state.calls.list_threads += 1;
        let mut items = state
            .created_threads
            .keys()
            .map(|thread_id| {
                stored_thread_from_state(&state, *thread_id, /*include_history*/ false)
            })
            .collect::<ThreadStoreResult<Vec<_>>>()?;
        items.sort_by_key(|item| item.thread_id.to_string());
        Ok(ThreadPage {
            items,
            next_cursor: None,
        })
    }

    async fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreResult<StoredThread> {
        if params.patch.project_id.is_some() {
            return Err(ThreadStoreError::Unsupported {
                operation: "projects",
            });
        }
        let mut state = self.state.lock().await;
        state.calls.update_thread_metadata += 1;
        if !state.created_threads.contains_key(&params.thread_id) {
            return Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            });
        }
        if let Some(name) = params.patch.name.clone() {
            state.names.insert(params.thread_id, name);
        }
        state
            .metadata_updates
            .entry(params.thread_id)
            .or_default()
            .merge(params.patch);
        stored_thread_from_state(&state, params.thread_id, /*include_history*/ false)
    }

    async fn move_thread_to_section(
        &self,
        params: MoveThreadToSectionParams,
    ) -> ThreadStoreResult<()> {
        if params
            .section
            .as_deref()
            .is_some_and(|section| section.trim().is_empty())
        {
            return Err(ThreadStoreError::InvalidRequest {
                message: "section must not be empty".to_owned(),
            });
        }
        if params.section.is_none() && params.before_thread_id.is_some() {
            return Err(ThreadStoreError::InvalidRequest {
                message: "before thread cannot be specified without a section".to_owned(),
            });
        }

        let mut state = self.state.lock().await;
        if !state.created_threads.contains_key(&params.thread_id) {
            return Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            });
        }
        let previous_section = state.sections.get(&params.thread_id).cloned();
        let Some(section) = params.section.as_deref() else {
            state.sections.remove(&params.thread_id);
            state.section_positions.remove(&params.thread_id);
            state.section_entered_at.remove(&params.thread_id);
            return Ok(());
        };
        if let Some(before_thread_id) = params.before_thread_id {
            if before_thread_id == params.thread_id {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!("thread {} cannot be moved before itself", params.thread_id),
                });
            }
            let before_section = state.sections.get(&before_thread_id).map(String::as_str);
            if before_section != Some(section) {
                return Err(ThreadStoreError::InvalidRequest {
                    message: format!(
                        "before thread {before_thread_id} is not in section {section}"
                    ),
                });
            }
        }

        let mut ordered_thread_ids = state
            .sections
            .iter()
            .filter(|(thread_id, current_section)| {
                **thread_id != params.thread_id && current_section.as_str() == section
            })
            .map(|(thread_id, _)| *thread_id)
            .collect::<Vec<_>>();
        ordered_thread_ids.sort_by_key(|thread_id| {
            (
                state
                    .section_positions
                    .get(thread_id)
                    .copied()
                    .unwrap_or(i64::MAX),
                thread_id.to_string(),
            )
        });
        let insert_at = params
            .before_thread_id
            .and_then(|before_thread_id| {
                ordered_thread_ids
                    .iter()
                    .position(|thread_id| *thread_id == before_thread_id)
            })
            .unwrap_or(ordered_thread_ids.len());
        ordered_thread_ids.insert(insert_at, params.thread_id);
        for (index, thread_id) in ordered_thread_ids.into_iter().enumerate() {
            let position = i64::try_from(index)
                .unwrap_or(i64::MAX)
                .saturating_add(1)
                .saturating_mul(1_000_000);
            state.section_positions.insert(thread_id, position);
        }
        if previous_section.as_deref() != Some(section) {
            state
                .section_entered_at
                .insert(params.thread_id, Utc::now());
            state.sections.insert(params.thread_id, section.to_owned());
        }
        Ok(())
    }

    async fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreResult<()> {
        let mut state = self.state.lock().await;
        state.calls.delete_thread += 1;
        let existed = state.histories.remove(&params.thread_id).is_some();
        state.created_threads.remove(&params.thread_id);
        state.names.remove(&params.thread_id);
        state.metadata_updates.remove(&params.thread_id);
        state.sections.remove(&params.thread_id);
        state.execution_accounts.remove(&params.thread_id);
        state
            .turn_execution_accounts
            .retain(|(thread_id, _), _| *thread_id != params.thread_id);
        state.section_positions.remove(&params.thread_id);
        state.section_entered_at.remove(&params.thread_id);
        state
            .rollout_paths
            .retain(|_, thread_id| *thread_id != params.thread_id);
        if existed {
            Ok(())
        } else {
            Err(ThreadStoreError::ThreadNotFound {
                thread_id: params.thread_id,
            })
        }
    }
}

impl ThreadStore for InMemoryThreadStore {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn runtime_snapshot(
        &self,
        thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, ThreadStoreRuntimeSnapshot> {
        Box::pin(async move {
            let state = self.state.lock().await;
            let persisted = state.histories.contains_key(&thread_id);
            Ok(ThreadStoreRuntimeSnapshot {
                writer_ownership: if state.created_threads.contains_key(&thread_id) {
                    RuntimeWriterOwnership::OwnedHere
                } else {
                    RuntimeWriterOwnership::None
                },
                writer_store_id: None,
                writer_generation: None,
                writer_deny_reason: Some(
                    "persistent writer control requires the state database".to_string(),
                ),
                jsonl: None,
                sqlite: None,
                lag: None,
                flush_health: if persisted {
                    RuntimePersistenceHealth::Healthy
                } else {
                    RuntimePersistenceHealth::Unknown
                },
                materialize_health: if persisted {
                    RuntimePersistenceHealth::Healthy
                } else {
                    RuntimePersistenceHealth::Unknown
                },
                flushed_at: None,
                materialized_at: None,
                persistence_deny_reason: persisted
                    .then(|| "in-memory store has no durable position authority".to_string()),
            })
        })
    }

    fn execution_account_binding(
        &self,
        thread_id: ThreadId,
    ) -> ThreadStoreFuture<'_, Option<ExecutionAccountBinding>> {
        Box::pin(async move {
            Ok(self
                .state
                .lock()
                .await
                .execution_accounts
                .get(&thread_id)
                .cloned())
        })
    }

    fn initialize_execution_account_binding(
        &self,
        thread_id: ThreadId,
        initial: ExecutionAccountBinding,
    ) -> ThreadStoreFuture<'_, ExecutionAccountBinding> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            Ok(state
                .execution_accounts
                .entry(thread_id)
                .or_insert(initial)
                .clone())
        })
    }

    fn compare_and_swap_execution_account_binding(
        &self,
        thread_id: ThreadId,
        expected: ExecutionAccountBinding,
        next_slot_id: String,
    ) -> ThreadStoreFuture<'_, Option<ExecutionAccountBinding>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            let Some(current) = state.execution_accounts.get_mut(&thread_id) else {
                return Ok(None);
            };
            if current != &expected {
                return Ok(None);
            }
            let Some(generation) = current.generation.checked_add(1) else {
                return Err(ThreadStoreError::Internal {
                    message: "execution account generation overflow".to_string(),
                });
            };
            *current = ExecutionAccountBinding {
                slot_id: next_slot_id,
                generation,
            };
            Ok(Some(current.clone()))
        })
    }

    fn execution_account_slot_runtime_state(
        &self,
        slot_id: String,
    ) -> ThreadStoreFuture<'_, (u64, Vec<(ThreadId, ExecutionAccountBinding)>)> {
        Box::pin(async move {
            let state = self.state.lock().await;
            let runtime_version = state
                .account_slot_runtime_versions
                .get(&slot_id)
                .copied()
                .unwrap_or_default();
            let mut bindings = state
                .execution_accounts
                .iter()
                .filter(|(_, binding)| binding.slot_id == slot_id)
                .map(|(thread_id, binding)| (*thread_id, binding.clone()))
                .collect::<Vec<_>>();
            bindings.sort_by_key(|(thread_id, _)| thread_id.to_string());
            Ok((runtime_version, bindings))
        })
    }

    fn compare_and_swap_execution_account_slot_runtime(
        &self,
        slot_id: String,
        expected_runtime_version: u64,
        mut expected_bindings: Vec<(ThreadId, ExecutionAccountBinding)>,
    ) -> ThreadStoreFuture<'_, Option<(u64, Vec<(ThreadId, ExecutionAccountBinding)>)>> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            let current_runtime_version = state
                .account_slot_runtime_versions
                .get(&slot_id)
                .copied()
                .unwrap_or_default();
            let mut current_bindings = state
                .execution_accounts
                .iter()
                .filter(|(_, binding)| binding.slot_id == slot_id)
                .map(|(thread_id, binding)| (*thread_id, binding.clone()))
                .collect::<Vec<_>>();
            current_bindings.sort_by_key(|(thread_id, _)| thread_id.to_string());
            expected_bindings.sort_by_key(|(thread_id, _)| thread_id.to_string());
            if current_runtime_version != expected_runtime_version
                || current_bindings != expected_bindings
            {
                return Ok(None);
            }

            let Some(next_runtime_version) = expected_runtime_version.checked_add(1) else {
                return Err(ThreadStoreError::Internal {
                    message: "execution account runtime version overflow".to_string(),
                });
            };
            let mut next_bindings = Vec::with_capacity(expected_bindings.len());
            for (thread_id, binding) in &expected_bindings {
                let Some(generation) = binding.generation.checked_add(1) else {
                    return Err(ThreadStoreError::Internal {
                        message: "execution account generation overflow".to_string(),
                    });
                };
                next_bindings.push((
                    *thread_id,
                    ExecutionAccountBinding {
                        slot_id: slot_id.clone(),
                        generation,
                    },
                ));
            }

            state
                .account_slot_runtime_versions
                .insert(slot_id, next_runtime_version);
            for (thread_id, binding) in &next_bindings {
                state.execution_accounts.insert(*thread_id, binding.clone());
            }
            Ok(Some((next_runtime_version, next_bindings)))
        })
    }

    fn claim_thread_transition(
        &self,
        intent: ThreadTransitionIntent,
        reserved_current_thread_id: ThreadId,
        origin_instance_epoch: String,
        initiator_client_incarnation: String,
        previous_writer: ThreadWriterEvidence,
    ) -> ThreadStoreFuture<'_, ThreadTransitionClaimOutcome> {
        Box::pin(thread_transition::claim(
            self,
            intent,
            reserved_current_thread_id,
            origin_instance_epoch,
            initiator_client_incarnation,
            previous_writer,
        ))
    }

    fn mark_thread_transition_prepared(
        &self,
        request: MarkThreadTransitionPrepared,
    ) -> ThreadStoreFuture<'_, ThreadTransitionPreparation> {
        Box::pin(thread_transition::mark_prepared(self, request))
    }

    fn abort_thread_transition(
        &self,
        request: AbortThreadTransition,
    ) -> ThreadStoreFuture<'_, ThreadTransitionAbortOutcome> {
        Box::pin(thread_transition::abort(self, request))
    }

    fn commit_thread_transition(
        &self,
        request: CommitThreadTransition,
    ) -> ThreadStoreFuture<'_, ThreadTransitionCommitOutcome> {
        Box::pin(thread_transition::commit(self, request))
    }

    fn thread_transition_by_id(
        &self,
        transition_id: String,
    ) -> ThreadStoreFuture<'_, Option<ThreadTransitionRecord>> {
        Box::pin(thread_transition::by_id(self, transition_id))
    }

    fn committed_thread_transitions_for_threads(
        &self,
        thread_ids: Vec<ThreadId>,
    ) -> ThreadStoreFuture<'_, HashMap<ThreadId, CommittedThreadTransitions>> {
        Box::pin(thread_transition::committed_for_threads(self, thread_ids))
    }

    fn turn_execution_account(
        &self,
        thread_id: ThreadId,
        turn_id: String,
    ) -> ThreadStoreFuture<'_, Option<ExecutionAccountBinding>> {
        Box::pin(async move {
            Ok(self
                .state
                .lock()
                .await
                .turn_execution_accounts
                .get(&(thread_id, turn_id))
                .cloned())
        })
    }

    fn create_thread(&self, params: CreateThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::create_thread(self, params))
    }

    fn resume_thread(&self, params: ResumeThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::resume_thread(self, params))
    }

    fn append_items(&self, params: AppendThreadItemsParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::append_items(self, params))
    }

    fn persist_thread(
        &self,
        _thread_id: ThreadId,
        _context: PersistContext,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.persist_thread += 1;
            Ok(())
        })
    }

    fn flush_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.flush_thread += 1;
            Ok(())
        })
    }

    fn shutdown_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.shutdown_thread += 1;
            Ok(())
        })
    }

    fn relinquish_thread(
        &self,
        _thread_id: ThreadId,
        _expected_writer_generation: u64,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.calls.shutdown_thread += 1;
            Ok(())
        })
    }

    fn validate_writer_generation(
        &self,
        _thread_id: ThreadId,
        _expected_writer_generation: u64,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn discard_thread(&self, _thread_id: ThreadId) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.discard_thread += 1;
            Ok(())
        })
    }

    fn load_history(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredThreadHistory> {
        Box::pin(InMemoryThreadStore::load_history(self, params))
    }

    fn load_latest_model_context(
        &self,
        params: LoadThreadHistoryParams,
    ) -> ThreadStoreFuture<'_, StoredModelContext> {
        Box::pin(InMemoryThreadStore::load_latest_model_context(self, params))
    }

    fn read_thread(&self, params: ReadThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(InMemoryThreadStore::read_thread(self, params))
    }

    fn read_thread_by_rollout_path(
        &self,
        params: ReadThreadByRolloutPathParams,
    ) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(InMemoryThreadStore::read_thread_by_rollout_path(
            self, params,
        ))
    }

    fn list_threads(&self, params: ListThreadsParams) -> ThreadStoreFuture<'_, ThreadPage> {
        Box::pin(async move {
            let mut page = InMemoryThreadStore::list_threads(self).await?;
            match params.relation_filter {
                Some(ThreadRelationFilter::DirectChildrenOf(parent_thread_id)) => {
                    page.items
                        .retain(|thread| thread.parent_thread_id == Some(parent_thread_id));
                }
                Some(ThreadRelationFilter::DescendantsOf(ancestor_thread_id)) => {
                    let mut subtree = HashSet::from([ancestor_thread_id]);
                    loop {
                        let mut discovered = false;
                        for thread in &page.items {
                            if thread
                                .parent_thread_id
                                .is_some_and(|parent_thread_id| subtree.contains(&parent_thread_id))
                            {
                                discovered |= subtree.insert(thread.thread_id);
                            }
                        }
                        if !discovered {
                            break;
                        }
                    }
                    page.items.retain(|thread| {
                        thread.thread_id != ancestor_thread_id
                            && subtree.contains(&thread.thread_id)
                    });
                }
                None => {}
            }
            if let Some(section) = params.section.as_ref() {
                page.items.retain(|thread| {
                    thread.section.as_ref().map(|section| section.id.as_str()) == section.as_deref()
                });
            }
            if params.sort_key == ThreadSortKey::SectionPosition {
                page.items.sort_by_key(|thread| {
                    (
                        thread.section_position.unwrap_or(i64::MAX),
                        thread.thread_id.to_string(),
                    )
                });
                if params.sort_direction == crate::SortDirection::Desc {
                    page.items.reverse();
                }
            }
            Ok(page)
        })
    }

    fn update_thread_metadata(
        &self,
        params: UpdateThreadMetadataParams,
    ) -> ThreadStoreFuture<'_, Option<StoredThread>> {
        Box::pin(async move {
            let updated = InMemoryThreadStore::update_thread_metadata(self, params).await?;
            Ok((!self.omit_metadata_update_result.load(Ordering::Relaxed)).then_some(updated))
        })
    }

    fn move_thread_to_section(
        &self,
        params: MoveThreadToSectionParams,
    ) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::move_thread_to_section(self, params))
    }

    fn archive_thread(&self, _params: ArchiveThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(async move {
            self.state.lock().await.calls.archive_thread += 1;
            Ok(())
        })
    }

    fn unarchive_thread(&self, params: ArchiveThreadParams) -> ThreadStoreFuture<'_, StoredThread> {
        Box::pin(async move {
            let mut state = self.state.lock().await;
            state.calls.unarchive_thread += 1;
            stored_thread_from_state(&state, params.thread_id, /*include_history*/ false)
        })
    }

    fn delete_thread(&self, params: DeleteThreadParams) -> ThreadStoreFuture<'_, ()> {
        Box::pin(InMemoryThreadStore::delete_thread(self, params))
    }
}

fn stored_thread_from_state(
    state: &InMemoryThreadStoreState,
    thread_id: ThreadId,
    include_history: bool,
) -> ThreadStoreResult<StoredThread> {
    let created = state
        .created_threads
        .get(&thread_id)
        .ok_or(ThreadStoreError::ThreadNotFound { thread_id })?;
    let history_items = state.histories.get(&thread_id).cloned().unwrap_or_default();
    let history = include_history.then(|| StoredThreadHistory {
        thread_id,
        items: history_items.clone(),
    });
    let name = state.names.get(&thread_id).cloned().flatten();
    let metadata = state.metadata_updates.get(&thread_id);
    let rollout_path = state
        .rollout_paths
        .iter()
        .find_map(|(path, mapped_thread_id)| {
            (*mapped_thread_id == thread_id).then(|| path.clone())
        });

    Ok(StoredThread {
        thread_id,
        extra_config: created.extra_config.clone(),
        rollout_path: metadata
            .and_then(|metadata| metadata.rollout_path.clone())
            .or(rollout_path),
        forked_from_id: created.forked_from_id,
        parent_thread_id: created.parent_thread_id,
        preview: metadata
            .and_then(|metadata| metadata.preview.clone())
            .unwrap_or_default(),
        name,
        model_provider: metadata
            .and_then(|metadata| metadata.model_provider.clone())
            .unwrap_or_else(|| "test".to_string()),
        model: metadata.and_then(|metadata| metadata.model.clone()),
        reasoning_effort: metadata
            .and_then(|metadata| metadata.reasoning_effort.clone())
            .flatten(),
        created_at: metadata
            .and_then(|metadata| metadata.created_at)
            .unwrap_or_else(Utc::now),
        updated_at: metadata
            .and_then(|metadata| metadata.updated_at)
            .unwrap_or_else(Utc::now),
        recency_at: metadata
            .and_then(|metadata| metadata.advance_recency_at.or(metadata.updated_at))
            .unwrap_or_else(Utc::now),
        archived_at: None,
        section: state
            .sections
            .get(&thread_id)
            .cloned()
            .map(|id| codex_state::ThreadSection {
                name: if id == codex_state::PINNED_THREAD_SECTION_ID {
                    codex_state::PINNED_THREAD_SECTION_NAME.to_string()
                } else {
                    id.clone()
                },
                id,
                appearance: None,
            }),
        section_position: state.section_positions.get(&thread_id).copied(),
        section_entered_at: state.section_entered_at.get(&thread_id).copied(),
        project_id: None,
        cwd: metadata
            .and_then(|metadata| metadata.cwd.clone())
            .unwrap_or_default(),
        cli_version: metadata
            .and_then(|metadata| metadata.cli_version.clone())
            .unwrap_or_else(|| "test".to_string()),
        source: metadata
            .and_then(|metadata| metadata.source.clone())
            .unwrap_or_else(|| created.source.clone()),
        history_mode: created.history_mode,
        thread_source: metadata
            .and_then(|metadata| metadata.thread_source.clone())
            .unwrap_or_else(|| created.thread_source.clone()),
        agent_nickname: metadata.and_then(|metadata| metadata.agent_nickname.clone().flatten()),
        agent_role: metadata.and_then(|metadata| metadata.agent_role.clone().flatten()),
        agent_path: metadata.and_then(|metadata| metadata.agent_path.clone().flatten()),
        git_info: metadata.and_then(git_info_from_patch),
        approval_mode: metadata
            .and_then(|metadata| metadata.approval_mode)
            .unwrap_or(AskForApproval::Never),
        permission_profile: metadata
            .and_then(|metadata| metadata.permission_profile.clone())
            .unwrap_or_else(PermissionProfile::read_only),
        token_usage: metadata.and_then(|metadata| metadata.token_usage.clone()),
        first_user_message: metadata.and_then(|metadata| metadata.first_user_message.clone()),
        history,
    })
}

fn history_mode_from_state(
    state: &InMemoryThreadStoreState,
    thread_id: ThreadId,
) -> ThreadHistoryMode {
    state
        .created_threads
        .get(&thread_id)
        .map(|thread| thread.history_mode)
        .unwrap_or_default()
}

fn git_info_from_patch(patch: &ThreadMetadataPatch) -> Option<codex_protocol::protocol::GitInfo> {
    let git_info = patch.git_info.as_ref()?;
    let sha = git_info.sha.clone().flatten();
    let branch = git_info.branch.clone().flatten();
    let origin_url = git_info.origin_url.clone().flatten();
    if sha.is_none() && branch.is_none() && origin_url.is_none() {
        return None;
    }
    Some(codex_protocol::protocol::GitInfo {
        commit_hash: sha.as_deref().map(codex_git_utils::GitSha::new),
        branch,
        repository_url: origin_url,
    })
}
