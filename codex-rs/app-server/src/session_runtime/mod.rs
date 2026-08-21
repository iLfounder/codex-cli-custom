#[allow(dead_code)]
mod operations;
mod pagination;
mod relinquish;
mod snapshot;
mod switch_account;

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::SessionRuntimeChangedNotification;
use codex_app_server_protocol::SessionRuntimeListParams;
use codex_app_server_protocol::SessionRuntimeListResponse;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::ThreadStore;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;

use self::operations::OperationCache;
use self::pagination::CachedSnapshot;
use crate::account_registry::AccountRegistry;
use crate::error_code::internal_error;
use crate::outgoing_message::OutgoingMessageSender;
use crate::thread_state::ThreadStateManager;
use crate::thread_status::ThreadWatchManager;

const BUILD_ATTEMPTS: usize = 4;

pub(crate) struct SessionRuntimeEngine {
    instance_epoch: String,
    dirty_generation: AtomicU64,
    build_lock: Semaphore,
    state: Mutex<EngineState>,
    thread_store: Arc<dyn ThreadStore>,
    thread_manager: Arc<ThreadManager>,
    thread_state_manager: ThreadStateManager,
    thread_watch_manager: ThreadWatchManager,
    pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
    thread_list_state_permit: Arc<Semaphore>,
    account_registry: Arc<AccountRegistry>,
    outgoing: Arc<OutgoingMessageSender>,
}

#[derive(Default)]
struct EngineState {
    sequence: u64,
    threads: HashMap<ThreadId, RuntimeThreadState>,
    listed_threads: HashSet<ThreadId>,
    pages: pagination::SnapshotCache,
    #[allow(dead_code)]
    operations: OperationCache,
    switching_accounts: HashMap<ThreadId, String>,
}

#[derive(Default)]
struct RuntimeThreadState {
    revision: u64,
    last_activity_at: Option<i64>,
    last_snapshot: Option<SessionRuntimeSnapshot>,
}

impl SessionRuntimeEngine {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        thread_store: Arc<dyn ThreadStore>,
        thread_manager: Arc<ThreadManager>,
        thread_state_manager: ThreadStateManager,
        thread_watch_manager: ThreadWatchManager,
        pending_thread_unloads: Arc<Mutex<HashSet<ThreadId>>>,
        thread_list_state_permit: Arc<Semaphore>,
        account_registry: Arc<AccountRegistry>,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> Self {
        Self {
            instance_epoch: uuid::Uuid::new_v4().to_string(),
            dirty_generation: AtomicU64::new(0),
            build_lock: Semaphore::new(/*permits*/ 1),
            state: Mutex::new(EngineState::default()),
            thread_store,
            thread_manager,
            thread_state_manager,
            thread_watch_manager,
            pending_thread_unloads,
            thread_list_state_permit,
            account_registry,
            outgoing,
        }
    }

    pub(crate) fn mark_dirty(&self) {
        self.dirty_generation.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) async fn list(
        &self,
        params: SessionRuntimeListParams,
    ) -> Result<SessionRuntimeListResponse, JSONRPCErrorError> {
        let limit = pagination::requested_limit(params.limit)?;
        if let Some(cursor) = params.cursor.as_deref() {
            if params.thread_id.is_some() {
                return Err(crate::error_code::invalid_params(
                    "sessionRuntime/list threadId cannot be combined with cursor",
                ));
            }
            return self.list_cached(cursor, limit).await;
        }

        let _build_guard = self
            .build_lock
            .acquire()
            .await
            .map_err(|_| internal_error("session runtime publisher is unavailable"))?;
        let mut snapshots = self
            .build_consistent_snapshots(params.thread_id.as_deref())
            .await?;
        let capabilities = self.process_capabilities().await;
        let mut state = self.state.lock().await;
        let mut changed = false;
        for snapshot in &mut snapshots {
            changed |= Self::apply_runtime_state(&mut state, snapshot, RuntimeActivity::Observe);
        }
        if params.thread_id.is_none() {
            let listed_threads = snapshots
                .iter()
                .filter_map(|snapshot| ThreadId::from_string(&snapshot.thread_id).ok())
                .collect::<HashSet<_>>();
            if state.listed_threads != listed_threads {
                state.listed_threads = listed_threads;
                changed = true;
            }
        }
        if changed {
            state.sequence = state.sequence.saturating_add(1);
        }
        let sequence = state.sequence;
        let cached = CachedSnapshot::new(sequence, snapshots);
        let response = cached.first_page(&self.instance_epoch, limit, capabilities)?;
        if response.next_cursor.is_some() {
            state.pages.insert(cached);
        } else {
            state.pages.clear();
        }
        Ok(response)
    }

    async fn list_cached(
        &self,
        cursor: &str,
        limit: usize,
    ) -> Result<SessionRuntimeListResponse, JSONRPCErrorError> {
        let capabilities = self.process_capabilities().await;
        let state = self.state.lock().await;
        state.pages.page(
            cursor,
            &self.instance_epoch,
            state.sequence,
            limit,
            capabilities,
        )
    }

    pub(crate) async fn publish_thread(&self, thread_id: ThreadId) {
        self.mark_dirty();
        let Ok(_build_guard) = self.build_lock.acquire().await else {
            return;
        };
        let thread_id_text = thread_id.to_string();
        let Ok(mut snapshots) = self
            .build_consistent_snapshots(Some(thread_id_text.as_str()))
            .await
        else {
            return;
        };
        let Some(mut snapshot) = snapshots.pop() else {
            return;
        };
        let notification = {
            let mut state = self.state.lock().await;
            Self::apply_runtime_state(&mut state, &mut snapshot, RuntimeActivity::Activity);
            state.sequence = state.sequence.saturating_add(1);
            state.pages.clear();
            SessionRuntimeChangedNotification {
                instance_epoch: self.instance_epoch.clone(),
                sequence: state.sequence,
                snapshot,
            }
        };
        self.outgoing
            .send_server_notification(ServerNotification::SessionRuntimeChanged(notification))
            .await;
    }

    pub(crate) async fn account_slot_in_use(
        &self,
        account_slot_id: &str,
    ) -> Result<bool, JSONRPCErrorError> {
        if let Some(local_store) = self
            .thread_store
            .as_any()
            .downcast_ref::<LocalThreadStore>()
            && local_store
                .durable_execution_account_slot_in_use(account_slot_id)
                .await
                .map_err(|_| internal_error("execution account binding store is unavailable"))?
        {
            return Ok(true);
        }
        let _build_guard = self
            .build_lock
            .acquire()
            .await
            .map_err(|_| internal_error("session runtime publisher is unavailable"))?;
        let snapshots = self.build_consistent_snapshots(/*thread_id*/ None).await?;
        Ok(snapshots.iter().any(|snapshot| {
            snapshot
                .account
                .current
                .as_ref()
                .is_some_and(|account| account.account_slot_id == account_slot_id)
                || snapshot
                    .account
                    .active_turn
                    .as_ref()
                    .is_some_and(|account| account.account_slot_id == account_slot_id)
                || snapshot.account.switch_target_slot_id.as_deref() == Some(account_slot_id)
        }))
    }

    async fn build_consistent_snapshots(
        &self,
        thread_id: Option<&str>,
    ) -> Result<Vec<SessionRuntimeSnapshot>, JSONRPCErrorError> {
        for _ in 0..BUILD_ATTEMPTS {
            let generation = self.dirty_generation.load(Ordering::Acquire);
            let records = self.runtime_records(thread_id).await?;
            let mut snapshots = Vec::with_capacity(records.len());
            for record in records {
                snapshots.push(self.build_snapshot(record).await);
            }
            snapshots.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
            if generation == self.dirty_generation.load(Ordering::Acquire) {
                return Ok(snapshots);
            }
        }
        Err(crate::error_code::invalid_params(
            "session runtime changed while building snapshot; retry",
        ))
    }

    fn apply_runtime_state(
        state: &mut EngineState,
        snapshot: &mut SessionRuntimeSnapshot,
        activity: RuntimeActivity,
    ) -> bool {
        let Ok(thread_id) = ThreadId::from_string(&snapshot.thread_id) else {
            return false;
        };
        let runtime = state.threads.entry(thread_id).or_default();
        match activity {
            RuntimeActivity::Activity => runtime.last_activity_at = Some(unix_timestamp()),
            RuntimeActivity::Observe if runtime.last_activity_at.is_none() => {
                runtime.last_activity_at = snapshot.lifecycle.last_activity_at;
            }
            RuntimeActivity::Observe => {}
        }
        snapshot.lifecycle.last_activity_at = runtime.last_activity_at;
        let mut comparable = snapshot.clone();
        comparable.state_revision = 0;
        let changed = runtime.last_snapshot.as_ref() != Some(&comparable);
        if changed {
            runtime.revision = runtime.revision.saturating_add(1).max(1);
            runtime.last_snapshot = Some(comparable);
        }
        snapshot.state_revision = runtime.revision;
        changed
    }
}

#[derive(Clone, Copy)]
enum RuntimeActivity {
    Observe,
    Activity,
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
