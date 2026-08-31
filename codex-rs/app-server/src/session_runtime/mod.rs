mod account_rotation;
pub(crate) mod continuity;
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
use codex_app_server_protocol::ThreadStatus;
use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use codex_protocol::protocol::RateLimitSnapshot as CoreRateLimitSnapshot;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::ThreadStore;
use tokio::sync::Mutex;
use tokio::sync::Semaphore;

use self::operations::OperationCache;
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
    source_generation: u64,
    threads: HashMap<ThreadId, RuntimeThreadState>,
    revisions: HashMap<ThreadId, u64>,
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

    pub(crate) fn instance_epoch(&self) -> &str {
        &self.instance_epoch
    }

    pub(crate) async fn publish_transition(
        &self,
        transition: codex_app_server_protocol::ThreadTransitionReceipt,
    ) {
        let sequence = {
            let mut state = self.state.lock().await;
            state.sequence = state.sequence.saturating_add(1);
            state.pages.clear();
            state.sequence
        };
        self.outgoing
            .send_server_notification(ServerNotification::ThreadTransitioned(
                codex_app_server_protocol::ThreadTransitionedNotification {
                    instance_epoch: self.instance_epoch.clone(),
                    sequence,
                    transition,
                },
            ))
            .await;
    }

    pub(crate) async fn list(
        &self,
        params: SessionRuntimeListParams,
    ) -> Result<SessionRuntimeListResponse, JSONRPCErrorError> {
        let limit = pagination::requested_limit(params.limit)?;
        if params.cursor.is_some() && params.thread_id.is_some() {
            return Err(crate::error_code::invalid_params(
                "sessionRuntime/list threadId cannot be combined with cursor",
            ));
        }

        let _build_guard = self
            .build_lock
            .acquire()
            .await
            .map_err(|_| internal_error("session runtime publisher is unavailable"))?;
        if let Some(thread_id) = params.thread_id.as_deref() {
            return self.list_exact(thread_id, limit).await;
        }
        if params.cursor.is_none() {
            self.refresh_inventory().await?;
        }
        self.materialize_page(params.cursor.as_deref(), limit).await
    }

    async fn list_exact(
        &self,
        thread_id: &str,
        limit: usize,
    ) -> Result<SessionRuntimeListResponse, JSONRPCErrorError> {
        let mut snapshots = self.build_consistent_snapshots(Some(thread_id)).await?;
        let current_source_generation = self.dirty_generation.load(Ordering::Acquire);
        let capabilities = self.process_capabilities().await;
        let mut state = self.state.lock().await;
        let source_changed = Self::sync_source_generation(&mut state, current_source_generation);
        let mut snapshot_changed = false;
        for snapshot in &mut snapshots {
            snapshot_changed |=
                Self::apply_runtime_state(&mut state, snapshot, RuntimeActivity::Observe);
        }
        if snapshot_changed && !source_changed {
            state.sequence = state.sequence.saturating_add(1);
            state.pages.clear();
        }
        let exact_thread_id = snapshots
            .first()
            .and_then(|snapshot| ThreadId::from_string(&snapshot.thread_id).ok());
        Self::retain_runtime_states(&mut state, exact_thread_id);
        Ok(SessionRuntimeListResponse {
            data: snapshots.into_iter().take(limit).collect(),
            next_cursor: None,
            instance_epoch: self.instance_epoch.clone(),
            snapshot_sequence: state.sequence,
            capabilities,
        })
    }

    async fn refresh_inventory(&self) -> Result<(), JSONRPCErrorError> {
        for _ in 0..BUILD_ATTEMPTS {
            let source_generation = self.dirty_generation.load(Ordering::Acquire);
            let inventory = self.runtime_inventory().await?;
            if source_generation != self.dirty_generation.load(Ordering::Acquire) {
                continue;
            }
            let mut state = self.state.lock().await;
            let source_changed = Self::sync_source_generation(&mut state, source_generation);
            if source_generation != self.dirty_generation.load(Ordering::Acquire) {
                continue;
            }
            if !state.pages.matches(source_generation, &inventory) {
                if !source_changed {
                    state.sequence = state.sequence.saturating_add(1);
                }
                let sequence = state.sequence;
                state.pages.replace(sequence, source_generation, inventory);
                Self::retain_runtime_states(&mut state, /*exact_thread_id*/ None);
            }
            return Ok(());
        }
        Err(crate::error_code::invalid_params(
            "session runtime changed while building inventory; retry",
        ))
    }

    async fn materialize_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<SessionRuntimeListResponse, JSONRPCErrorError> {
        let source_generation = self.dirty_generation.load(Ordering::Acquire);
        let plan = {
            let mut state = self.state.lock().await;
            if Self::sync_source_generation(&mut state, source_generation) {
                return Err(pagination::stale_cursor());
            }
            state.pages.plan(
                cursor,
                &self.instance_epoch,
                state.sequence,
                source_generation,
                limit,
            )?
        };
        let mut snapshots = Vec::with_capacity(plan.records.len());
        for record in &plan.records {
            snapshots.push(
                self.build_snapshot(record.clone(), Some(&plan.overlay))
                    .await,
            );
        }
        let capabilities = self.process_capabilities().await;
        let mut state = self.state.lock().await;
        let current_source_generation = self.dirty_generation.load(Ordering::Acquire);
        if source_generation != current_source_generation {
            Self::sync_source_generation(&mut state, current_source_generation);
            return Err(pagination::stale_cursor());
        }
        for snapshot in &mut snapshots {
            Self::apply_runtime_state(&mut state, snapshot, RuntimeActivity::Observe);
        }
        state.pages.commit(&plan, snapshots)?;
        let response = state
            .pages
            .response(&plan, &self.instance_epoch, limit, capabilities)?;
        Self::retain_runtime_states(&mut state, /*exact_thread_id*/ None);
        Ok(response)
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
            Self::sync_source_generation(&mut state, self.dirty_generation.load(Ordering::Acquire));
            Self::apply_runtime_state(&mut state, &mut snapshot, RuntimeActivity::Activity);
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
        let _build_guard = self
            .build_lock
            .acquire()
            .await
            .map_err(|_| internal_error("session runtime publisher is unavailable"))?;
        if let Some(local_store) = self
            .thread_store
            .as_any()
            .downcast_ref::<LocalThreadStore>()
        {
            if local_store
                .durable_execution_account_slot_in_use(account_slot_id)
                .await
                .map_err(|_| internal_error("execution account binding store is unavailable"))?
            {
                return Ok(true);
            }
        } else {
            for record in self.runtime_records(/*exact_thread_id*/ None).await? {
                if self
                    .thread_store
                    .execution_account_binding(record.thread_id())
                    .await
                    .map_err(|_| internal_error("execution account binding store is unavailable"))?
                    .is_some_and(|binding| binding.slot_id == account_slot_id)
                {
                    return Ok(true);
                }
            }
        }

        let switching_accounts = self.state.lock().await.switching_accounts.clone();
        for thread_id in self.thread_manager.list_thread_ids().await {
            if switching_accounts.get(&thread_id).map(String::as_str) == Some(account_slot_id) {
                return Ok(true);
            }
            let Ok(thread) = self.thread_manager.get_thread(thread_id).await else {
                continue;
            };
            if thread.execution_account().binding.slot_id == account_slot_id {
                return Ok(true);
            }
            let subscriptions = self.thread_state_manager.runtime_snapshot(thread_id).await;
            if let Some(turn_id) = subscriptions.active_turn_id
                && self
                    .thread_store
                    .turn_execution_account(thread_id, turn_id)
                    .await
                    .map_err(|_| internal_error("turn execution account store is unavailable"))?
                    .is_some_and(|binding| binding.slot_id == account_slot_id)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) async fn account_slot_has_active_turn(
        &self,
        account_slot_id: &str,
    ) -> Result<bool, JSONRPCErrorError> {
        let _build_guard = self
            .build_lock
            .acquire()
            .await
            .map_err(|_| internal_error("session runtime publisher is unavailable"))?;
        for thread_id in self.thread_manager.list_thread_ids().await {
            let status = self
                .thread_watch_manager
                .loaded_status_for_thread(&thread_id.to_string())
                .await;
            if !is_actual_active_status(&status) {
                continue;
            }
            let subscriptions = self.thread_state_manager.runtime_snapshot(thread_id).await;
            let Ok(thread) = self.thread_manager.get_thread(thread_id).await else {
                continue;
            };
            let turn_binding = match subscriptions.active_turn_id {
                Some(turn_id) => self
                    .thread_store
                    .turn_execution_account(thread_id, turn_id)
                    .await
                    .map_err(|_| internal_error("turn execution account store is unavailable"))?,
                None => None,
            };
            if active_status_matches_account_slot(
                &status,
                turn_binding.as_ref(),
                &thread.execution_account().binding,
                account_slot_id,
            ) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) async fn observe_rate_limit_update(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
        rate_limits: &CoreRateLimitSnapshot,
    ) {
        let Ok(Some(turn_binding)) = self
            .thread_store
            .turn_execution_account(thread_id, turn_id.to_string())
            .await
        else {
            return;
        };
        let Ok(Some(current_binding)) =
            self.thread_store.execution_account_binding(thread_id).await
        else {
            return;
        };
        if turn_binding != current_binding {
            return;
        }
        self.account_registry
            .invalidate_slot_quota(&current_binding.slot_id)
            .await;
        if rate_limits.spend_control_reached == Some(true)
            || rate_limits.rate_limit_reached_type.is_some()
        {
            self.account_registry
                .record_exhaustion_hint(crate::account_registry::rotation::ExhaustionHintKey {
                    thread_id,
                    account_slot_id: current_binding.slot_id,
                    execution_generation: current_binding.generation,
                })
                .await;
        }
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
                snapshots.push(self.build_snapshot(record, /*overlay*/ None).await);
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
        let EngineState {
            threads, revisions, ..
        } = state;
        let runtime = threads.entry(thread_id).or_default();
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
        let revision = revisions.entry(thread_id).or_default();
        if changed {
            *revision = revision.saturating_add(1).max(1);
            runtime.revision = *revision;
            runtime.last_snapshot = Some(comparable);
        }
        runtime.revision = *revision;
        snapshot.state_revision = *revision;
        changed
    }

    fn sync_source_generation(state: &mut EngineState, source_generation: u64) -> bool {
        if state.source_generation == source_generation {
            return false;
        }
        state.source_generation = source_generation;
        state.sequence = state.sequence.saturating_add(1);
        state.pages.clear();
        true
    }

    fn retain_runtime_states(state: &mut EngineState, exact_thread_id: Option<ThreadId>) {
        let mut retained = state.pages.retained_thread_ids();
        retained.extend(exact_thread_id);
        retained.extend(state.switching_accounts.keys().copied());
        retained.extend(
            state
                .operations
                .operations
                .values()
                .filter_map(|operation| {
                    operation
                        .thread_id
                        .as_deref()
                        .and_then(|thread_id| ThreadId::from_string(thread_id).ok())
                }),
        );
        state
            .threads
            .retain(|thread_id, _runtime| retained.contains(thread_id));
        retained.extend(state.pages.inventory_thread_ids());
        state
            .revisions
            .retain(|thread_id, _revision| retained.contains(thread_id));
    }
}

fn is_actual_active_status(status: &ThreadStatus) -> bool {
    matches!(status, ThreadStatus::Active { .. })
}

fn active_status_matches_account_slot(
    status: &ThreadStatus,
    active_turn_binding: Option<&ExecutionAccountBinding>,
    current_binding: &ExecutionAccountBinding,
    account_slot_id: &str,
) -> bool {
    is_actual_active_status(status)
        && active_turn_binding
            .unwrap_or(current_binding)
            .slot_id
            .eq(account_slot_id)
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
