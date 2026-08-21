use std::collections::HashSet;

use codex_app_server_protocol::GitInfo;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::SessionRuntimeAccountBinding;
use codex_app_server_protocol::SessionRuntimeAccountRef;
use codex_app_server_protocol::SessionRuntimeAccountSwitchState;
use codex_app_server_protocol::SessionRuntimeAction;
use codex_app_server_protocol::SessionRuntimeActionAvailability;
use codex_app_server_protocol::SessionRuntimeCapability;
use codex_app_server_protocol::SessionRuntimeIdentity;
use codex_app_server_protocol::SessionRuntimeLifecycle;
use codex_app_server_protocol::SessionRuntimeLifecycleState;
use codex_app_server_protocol::SessionRuntimePersistence;
use codex_app_server_protocol::SessionRuntimePersistenceHealth;
use codex_app_server_protocol::SessionRuntimePersistencePosition;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use codex_app_server_protocol::SessionRuntimeWaitingOn;
use codex_app_server_protocol::SessionRuntimeWriter;
use codex_app_server_protocol::SessionRuntimeWriterState;
use codex_app_server_protocol::ThreadActiveFlag;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_thread_store::ListThreadsParams;
use codex_thread_store::ReadThreadParams;
use codex_thread_store::RuntimePersistenceHealth;
use codex_thread_store::RuntimePersistencePosition;
use codex_thread_store::RuntimeWriterOwnership;
use codex_thread_store::SortDirection;
use codex_thread_store::StoredThread;
use codex_thread_store::ThreadSortKey;
use codex_thread_store::ThreadStoreError;
use codex_thread_store::WriterControlCapability;

use super::SessionRuntimeEngine;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;
use crate::request_processors::thread_settings_from_config_snapshot;
use crate::thread_status::resolve_thread_status;

const STORE_PAGE_SIZE: usize = 200;
const IDENTITY_CAPABILITY: &str = "ananke_session_identity_v1";
const CONTROL_CAPABILITY: &str = "ananke_session_control_v1";
const CONTROL_NOT_IMPLEMENTED: &str = "control_not_implemented";
const RUNTIME_SNAPSHOT_UNAVAILABLE: &str = "thread store runtime state is unavailable";

pub(super) enum RuntimeRecord {
    Stored(Box<StoredThread>),
    LoadedOnly {
        thread_id: ThreadId,
        session_id: String,
        source: SessionSource,
        cwd: String,
        forked_from_id: Option<ThreadId>,
        parent_thread_id: Option<ThreadId>,
    },
}

impl SessionRuntimeEngine {
    pub(super) async fn runtime_records(
        &self,
        exact_thread_id: Option<&str>,
    ) -> Result<Vec<RuntimeRecord>, JSONRPCErrorError> {
        if let Some(thread_id) = exact_thread_id {
            let thread_id = ThreadId::from_string(thread_id)
                .map_err(|_| invalid_params("sessionRuntime/list threadId is invalid"))?;
            return self
                .runtime_record(thread_id)
                .await
                .map(|record| record.into_iter().collect());
        }

        let mut stored = self.list_stored_threads().await?;
        let mut seen = stored
            .iter()
            .map(|thread| thread.thread_id)
            .collect::<HashSet<_>>();
        let mut records = stored
            .drain(..)
            .map(|thread| RuntimeRecord::Stored(Box::new(thread)))
            .collect::<Vec<_>>();
        for thread_id in self.thread_manager.list_thread_ids().await {
            if seen.insert(thread_id)
                && let Some(record) = self.loaded_record(thread_id).await
            {
                records.push(record);
            }
        }
        Ok(records)
    }

    async fn runtime_record(
        &self,
        thread_id: ThreadId,
    ) -> Result<Option<RuntimeRecord>, JSONRPCErrorError> {
        match self
            .thread_store
            .read_thread(ReadThreadParams {
                thread_id,
                include_archived: true,
                include_history: false,
            })
            .await
        {
            Ok(thread) => Ok(Some(RuntimeRecord::Stored(Box::new(thread)))),
            Err(ThreadStoreError::ThreadNotFound { .. }) => Ok(self.loaded_record(thread_id).await),
            Err(_) => Err(internal_error(
                "session runtime identity store is unavailable",
            )),
        }
    }

    async fn loaded_record(&self, thread_id: ThreadId) -> Option<RuntimeRecord> {
        let thread = self.thread_manager.get_thread(thread_id).await.ok()?;
        let configured = thread.session_configured();
        let config = thread.config_snapshot().await;
        Some(RuntimeRecord::LoadedOnly {
            thread_id,
            session_id: configured.session_id.to_string(),
            source: config.session_source.clone(),
            cwd: config.cwd().as_path().to_string_lossy().into_owned(),
            forked_from_id: config.forked_from_thread_id,
            parent_thread_id: config.parent_thread_id,
        })
    }

    async fn list_stored_threads(&self) -> Result<Vec<StoredThread>, JSONRPCErrorError> {
        let mut cursor = None;
        let mut threads = Vec::new();
        loop {
            let page = self
                .thread_store
                .list_threads(ListThreadsParams {
                    page_size: STORE_PAGE_SIZE,
                    cursor,
                    sort_key: ThreadSortKey::CreatedAt,
                    sort_direction: SortDirection::Asc,
                    allowed_sources: Vec::new(),
                    model_providers: Some(Vec::new()),
                    cwd_filters: None,
                    section: None,
                    project_id: None,
                    archived: false,
                    search_term: None,
                    relation_filter: None,
                    use_state_db_only: false,
                })
                .await
                .map_err(|_| internal_error("session runtime identity store is unavailable"))?;
            threads.extend(page.items);
            let Some(next_cursor) = page.next_cursor else {
                break;
            };
            cursor = Some(next_cursor);
        }
        Ok(threads)
    }

    pub(super) async fn build_snapshot(&self, record: RuntimeRecord) -> SessionRuntimeSnapshot {
        let thread_id = record.thread_id();
        let loaded_thread = self.thread_manager.get_thread(thread_id).await.ok();
        let (settings, loaded_session_id) = match loaded_thread.as_ref() {
            Some(thread) => {
                let configured = thread.session_configured();
                (
                    Some(thread_settings_from_config_snapshot(
                        &thread.config_snapshot().await,
                    )),
                    Some(configured.session_id.to_string()),
                )
            }
            None => (None, None),
        };
        let subscriptions = self.thread_state_manager.runtime_snapshot(thread_id).await;
        let mut status = self
            .thread_watch_manager
            .loaded_status_for_thread(&thread_id.to_string())
            .await;
        if let Some(thread) = loaded_thread.as_ref()
            && record.is_thread_spawn_subagent()
            && matches!(status, ThreadStatus::NotLoaded)
        {
            status = subagent_status(thread.agent_status().await);
        }
        let closing = self
            .pending_thread_unloads
            .lock()
            .await
            .contains(&thread_id);
        let pending_server_request = !self
            .outgoing
            .pending_requests_for_thread(thread_id)
            .await
            .is_empty();
        let store_runtime = self
            .thread_store
            .runtime_snapshot(thread_id)
            .await
            .unwrap_or_else(|_| {
                codex_thread_store::ThreadStoreRuntimeSnapshot::unavailable(
                    RUNTIME_SNAPSHOT_UNAVAILABLE,
                )
            });
        let current_binding = match loaded_thread.as_ref() {
            Some(thread) => Ok(Some(thread.execution_account().binding.clone())),
            None => self.thread_store.execution_account_binding(thread_id).await,
        };
        let active_binding = match subscriptions.active_turn_id.as_ref() {
            Some(turn_id) => self
                .thread_store
                .turn_execution_account(thread_id, turn_id.clone())
                .await
                .ok()
                .flatten(),
            None => None,
        };
        let account_capability = self.account_registry.runtime_capability().await.ok();
        let lifecycle = lifecycle_snapshot(
            status,
            loaded_thread.is_some(),
            closing,
            subscriptions.active_turn_id,
            subscriptions.subscriber_count,
            subscriptions.client_incarnations,
            pending_server_request,
            record.updated_at(),
            subscriptions.unload_at,
        );
        let writer = writer_snapshot(&store_runtime);
        let account =
            account_snapshot(current_binding, active_binding, account_capability.as_ref());
        let actions = action_snapshot(&lifecycle, &writer);
        SessionRuntimeSnapshot {
            thread_id: thread_id.to_string(),
            state_revision: 0,
            identity: record.identity(settings.or(subscriptions.settings), loaded_session_id),
            lifecycle,
            writer,
            persistence: persistence_snapshot(&store_runtime),
            account,
            actions,
        }
    }

    pub(super) async fn process_capabilities(&self) -> Vec<SessionRuntimeCapability> {
        let (available, deny_reason) = match self.thread_store.writer_control_capability() {
            WriterControlCapability::Enabled { .. } => (true, None),
            WriterControlCapability::Disabled { reason } => (false, Some(reason)),
        };
        vec![
            SessionRuntimeCapability {
                name: IDENTITY_CAPABILITY.to_string(),
                available: true,
                deny_reason: None,
            },
            SessionRuntimeCapability {
                name: CONTROL_CAPABILITY.to_string(),
                available,
                deny_reason,
            },
        ]
    }
}

impl RuntimeRecord {
    fn thread_id(&self) -> ThreadId {
        match self {
            Self::Stored(thread) => thread.thread_id,
            Self::LoadedOnly { thread_id, .. } => *thread_id,
        }
    }

    fn updated_at(&self) -> Option<i64> {
        match self {
            Self::Stored(thread) => Some(thread.updated_at.timestamp()),
            Self::LoadedOnly { .. } => None,
        }
    }

    fn is_thread_spawn_subagent(&self) -> bool {
        matches!(
            self.source(),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
        )
    }

    fn source(&self) -> &SessionSource {
        match self {
            Self::Stored(thread) => &thread.source,
            Self::LoadedOnly { source, .. } => source,
        }
    }

    fn identity(
        self,
        settings: Option<codex_app_server_protocol::ThreadSettings>,
        loaded_session_id: Option<String>,
    ) -> SessionRuntimeIdentity {
        match self {
            Self::Stored(thread) => SessionRuntimeIdentity {
                session_id: loaded_session_id.unwrap_or_else(|| thread.thread_id.to_string()),
                forked_from_id: thread.forked_from_id.map(|id| id.to_string()),
                parent_thread_id: thread.parent_thread_id.map(|id| id.to_string()),
                name: thread.name,
                source: thread.source.to_string(),
                cwd: thread.cwd.to_string_lossy().into_owned(),
                git_info: thread.git_info.map(|git| GitInfo {
                    sha: git.commit_hash.map(|sha| sha.0),
                    branch: git.branch,
                    origin_url: git.repository_url.and_then(sanitize_git_origin),
                }),
                settings,
            },
            Self::LoadedOnly {
                thread_id: _,
                session_id,
                source,
                cwd,
                forked_from_id,
                parent_thread_id,
            } => SessionRuntimeIdentity {
                session_id,
                forked_from_id: forked_from_id.map(|id| id.to_string()),
                parent_thread_id: parent_thread_id.map(|id| id.to_string()),
                name: None,
                source: source.to_string(),
                cwd,
                git_info: None,
                settings,
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn lifecycle_snapshot(
    status: ThreadStatus,
    loaded: bool,
    closing: bool,
    active_turn_id: Option<String>,
    subscriber_count: u32,
    client_incarnations: Vec<String>,
    pending_server_request: bool,
    last_activity_at: Option<i64>,
    unload_at: Option<i64>,
) -> SessionRuntimeLifecycle {
    let mut waiting_on = match &status {
        ThreadStatus::Active { active_flags } => active_flags
            .iter()
            .map(|flag| match flag {
                ThreadActiveFlag::WaitingOnApproval => SessionRuntimeWaitingOn::Approval,
                ThreadActiveFlag::WaitingOnUserInput => SessionRuntimeWaitingOn::UserInput,
            })
            .collect::<Vec<_>>(),
        ThreadStatus::NotLoaded | ThreadStatus::Idle | ThreadStatus::SystemError => Vec::new(),
    };
    if pending_server_request {
        waiting_on.push(SessionRuntimeWaitingOn::ServerRequest);
    }
    waiting_on.sort_by_key(|waiting| *waiting as u8);
    waiting_on.dedup();
    let state = if closing {
        SessionRuntimeLifecycleState::Closing
    } else {
        match status {
            ThreadStatus::NotLoaded if loaded => SessionRuntimeLifecycleState::Loaded,
            ThreadStatus::NotLoaded => SessionRuntimeLifecycleState::NotLoaded,
            ThreadStatus::SystemError if loaded => SessionRuntimeLifecycleState::Loaded,
            ThreadStatus::SystemError => SessionRuntimeLifecycleState::NotLoaded,
            ThreadStatus::Idle => SessionRuntimeLifecycleState::Idle,
            ThreadStatus::Active { .. } => SessionRuntimeLifecycleState::Active,
        }
    };
    SessionRuntimeLifecycle {
        state,
        active_turn_id,
        waiting_on,
        subscriber_count,
        client_incarnations,
        last_activity_at,
        unload_at,
    }
}

fn subagent_status(status: AgentStatus) -> ThreadStatus {
    match status {
        AgentStatus::Running => {
            resolve_thread_status(ThreadStatus::Idle, /*has_in_progress_turn*/ true)
        }
        AgentStatus::PendingInit | AgentStatus::Interrupted | AgentStatus::Completed(_) => {
            ThreadStatus::Idle
        }
        AgentStatus::Errored(_) => ThreadStatus::SystemError,
        AgentStatus::Shutdown | AgentStatus::NotFound => ThreadStatus::NotLoaded,
    }
}

pub(super) fn writer_snapshot(
    runtime: &codex_thread_store::ThreadStoreRuntimeSnapshot,
) -> SessionRuntimeWriter {
    SessionRuntimeWriter {
        state: match runtime.writer_ownership {
            RuntimeWriterOwnership::None => SessionRuntimeWriterState::None,
            RuntimeWriterOwnership::OwnedHere => SessionRuntimeWriterState::OwnedHere,
            RuntimeWriterOwnership::OwnedElsewhere => SessionRuntimeWriterState::OwnedElsewhere,
            RuntimeWriterOwnership::Unavailable => SessionRuntimeWriterState::Unavailable,
        },
        store_id: runtime.writer_store_id.clone(),
        writer_generation: runtime.writer_generation,
        deny_reason: runtime.writer_deny_reason.clone(),
    }
}

pub(super) fn persistence_snapshot(
    runtime: &codex_thread_store::ThreadStoreRuntimeSnapshot,
) -> SessionRuntimePersistence {
    SessionRuntimePersistence {
        jsonl: runtime.jsonl.map(persistence_position),
        sqlite: runtime.sqlite.map(persistence_position),
        lag: runtime.lag,
        flush_health: persistence_health(runtime.flush_health),
        materialize_health: persistence_health(runtime.materialize_health),
        flushed_at: runtime.flushed_at,
        materialized_at: runtime.materialized_at,
        deny_reason: runtime.persistence_deny_reason.clone(),
    }
}

fn persistence_position(position: RuntimePersistencePosition) -> SessionRuntimePersistencePosition {
    SessionRuntimePersistencePosition {
        ordinal: position.ordinal,
        offset: position.offset,
    }
}

fn persistence_health(health: RuntimePersistenceHealth) -> SessionRuntimePersistenceHealth {
    match health {
        RuntimePersistenceHealth::Unknown => SessionRuntimePersistenceHealth::Unknown,
        RuntimePersistenceHealth::Healthy => SessionRuntimePersistenceHealth::Healthy,
        RuntimePersistenceHealth::Degraded => SessionRuntimePersistenceHealth::Degraded,
        RuntimePersistenceHealth::Failed => SessionRuntimePersistenceHealth::Failed,
    }
}

fn account_snapshot(
    current: Result<Option<ExecutionAccountBinding>, ThreadStoreError>,
    active_turn: Option<ExecutionAccountBinding>,
    capability: Option<&codex_app_server_protocol::AccountSlotCapability>,
) -> SessionRuntimeAccountBinding {
    let (current, binding_error) = match current {
        Ok(current) => (current.map(account_ref), None),
        Err(_) => (
            None,
            Some("execution_account_binding_unavailable".to_string()),
        ),
    };
    let deny_reason = binding_error
        .or_else(|| capability.and_then(|capability| capability.deny_reason.clone()))
        .or_else(|| current.is_none().then(|| "account_unbound".to_string()));
    SessionRuntimeAccountBinding {
        switch_state: if current.is_none() {
            SessionRuntimeAccountSwitchState::Unbound
        } else if capability.is_none() {
            SessionRuntimeAccountSwitchState::Degraded
        } else {
            SessionRuntimeAccountSwitchState::Stable
        },
        current,
        active_turn: active_turn.map(account_ref),
        switch_target_slot_id: None,
        deny_reason,
    }
}

fn account_ref(binding: ExecutionAccountBinding) -> SessionRuntimeAccountRef {
    SessionRuntimeAccountRef {
        account_slot_id: binding.slot_id,
        execution_generation: binding.generation,
    }
}

pub(super) fn action_snapshot(
    lifecycle: &SessionRuntimeLifecycle,
    writer: &SessionRuntimeWriter,
) -> Vec<SessionRuntimeActionAvailability> {
    let relinquish_denial = if lifecycle.state != SessionRuntimeLifecycleState::Idle
        || lifecycle.active_turn_id.is_some()
        || !lifecycle.waiting_on.is_empty()
    {
        Some("thread_not_idle")
    } else if lifecycle.subscriber_count > 1 {
        Some("other_subscribers_present")
    } else if writer.state != SessionRuntimeWriterState::OwnedHere {
        Some("writer_not_owned")
    } else if writer.store_id.is_none() || writer.writer_generation.is_none() {
        Some("writer_control_unavailable")
    } else {
        None
    };
    vec![
        SessionRuntimeActionAvailability {
            action: SessionRuntimeAction::Relinquish,
            allowed: relinquish_denial.is_none(),
            deny_reason: relinquish_denial.map(str::to_string),
        },
        SessionRuntimeActionAvailability {
            action: SessionRuntimeAction::SwitchAccount,
            allowed: false,
            deny_reason: Some(CONTROL_NOT_IMPLEMENTED.to_string()),
        },
    ]
}

fn sanitize_git_origin(origin: String) -> Option<String> {
    let mut url = url::Url::parse(&origin).ok()?;
    if !matches!(url.scheme(), "http" | "https" | "ssh") || url.host().is_none() {
        return None;
    }
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}
