use std::collections::HashMap;
use std::collections::HashSet;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::SessionRuntimeCapability;
use codex_app_server_protocol::SessionRuntimeListResponse;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;

use super::snapshot::RuntimeInventory;
use super::snapshot::RuntimeOverlay;
use super::snapshot::RuntimeRecord;
use crate::error_code::internal_error;
use crate::error_code::invalid_params;

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 100;

#[derive(Default)]
pub(super) struct SnapshotCache {
    snapshot: Option<CachedSnapshot>,
}

pub(super) struct CachedSnapshot {
    id: String,
    sequence: u64,
    source_generation: u64,
    records: Vec<RuntimeRecord>,
    overlay: RuntimeOverlay,
    snapshots: HashMap<ThreadId, SessionRuntimeSnapshot>,
}

pub(super) struct PagePlan {
    snapshot_id: String,
    sequence: u64,
    source_generation: u64,
    start: usize,
    pub(super) records: Vec<RuntimeRecord>,
    pub(super) overlay: RuntimeOverlay,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimeCursor {
    instance_epoch: String,
    snapshot_sequence: u64,
    snapshot_id: String,
    after_thread_id: String,
}

impl SnapshotCache {
    pub(super) fn matches(&self, source_generation: u64, inventory: &RuntimeInventory) -> bool {
        self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.source_generation == source_generation
                && snapshot.overlay == inventory.overlay
                && snapshot.records.len() == inventory.records.len()
                && snapshot
                    .records
                    .iter()
                    .zip(&inventory.records)
                    .all(|(left, right)| left.same_inventory(right))
        })
    }

    pub(super) fn replace(
        &mut self,
        sequence: u64,
        source_generation: u64,
        inventory: RuntimeInventory,
    ) {
        self.snapshot = Some(CachedSnapshot {
            id: uuid::Uuid::new_v4().to_string(),
            sequence,
            source_generation,
            records: inventory.records,
            overlay: inventory.overlay,
            snapshots: HashMap::new(),
        });
    }

    pub(super) fn clear(&mut self) {
        self.snapshot = None;
    }

    pub(super) fn plan(
        &self,
        encoded_cursor: Option<&str>,
        instance_epoch: &str,
        current_sequence: u64,
        current_source_generation: u64,
        limit: usize,
    ) -> Result<PagePlan, JSONRPCErrorError> {
        let snapshot = self.snapshot.as_ref().ok_or_else(stale_cursor)?;
        if snapshot.sequence != current_sequence
            || snapshot.source_generation != current_source_generation
        {
            return Err(stale_cursor());
        }
        let start = if let Some(encoded_cursor) = encoded_cursor {
            let cursor = decode_cursor(encoded_cursor)?;
            if cursor.instance_epoch != instance_epoch
                || cursor.snapshot_sequence != current_sequence
                || cursor.snapshot_id != snapshot.id
            {
                return Err(stale_cursor());
            }
            snapshot
                .records
                .iter()
                .position(|record| record.thread_id().to_string() == cursor.after_thread_id)
                .map(|index| index + 1)
                .ok_or_else(|| invalid_params("sessionRuntime/list cursor is invalid"))?
        } else {
            0
        };
        let materialize_end = start
            .saturating_add(limit)
            .saturating_add(1)
            .min(snapshot.records.len());
        let records = snapshot.records[start..materialize_end]
            .iter()
            .filter(|record| !snapshot.snapshots.contains_key(&record.thread_id()))
            .cloned()
            .collect();
        Ok(PagePlan {
            snapshot_id: snapshot.id.clone(),
            sequence: snapshot.sequence,
            source_generation: snapshot.source_generation,
            start,
            records,
            overlay: snapshot.overlay.clone(),
        })
    }

    pub(super) fn commit(
        &mut self,
        plan: &PagePlan,
        snapshots: Vec<SessionRuntimeSnapshot>,
    ) -> Result<(), JSONRPCErrorError> {
        let snapshot = self.snapshot.as_mut().ok_or_else(stale_cursor)?;
        if snapshot.id != plan.snapshot_id
            || snapshot.sequence != plan.sequence
            || snapshot.source_generation != plan.source_generation
        {
            return Err(stale_cursor());
        }
        for snapshot_item in snapshots {
            let thread_id = ThreadId::from_string(&snapshot_item.thread_id)
                .map_err(|_| internal_error("session runtime snapshot thread id is invalid"))?;
            snapshot.snapshots.insert(thread_id, snapshot_item);
        }
        Ok(())
    }

    pub(super) fn response(
        &self,
        plan: &PagePlan,
        instance_epoch: &str,
        limit: usize,
        capabilities: Vec<SessionRuntimeCapability>,
    ) -> Result<SessionRuntimeListResponse, JSONRPCErrorError> {
        let snapshot = self.snapshot.as_ref().ok_or_else(stale_cursor)?;
        if snapshot.id != plan.snapshot_id
            || snapshot.sequence != plan.sequence
            || snapshot.source_generation != plan.source_generation
        {
            return Err(stale_cursor());
        }
        let end = plan.start.saturating_add(limit).min(snapshot.records.len());
        let data = snapshot.records[plan.start..end]
            .iter()
            .map(|record| {
                snapshot
                    .snapshots
                    .get(&record.thread_id())
                    .cloned()
                    .ok_or_else(|| internal_error("session runtime page was not materialized"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = if end == 0 || end >= snapshot.records.len() {
            None
        } else {
            Some(encode_cursor(&RuntimeCursor {
                instance_epoch: instance_epoch.to_string(),
                snapshot_sequence: snapshot.sequence,
                snapshot_id: snapshot.id.clone(),
                after_thread_id: snapshot.records[end - 1].thread_id().to_string(),
            })?)
        };
        Ok(SessionRuntimeListResponse {
            data,
            next_cursor,
            instance_epoch: instance_epoch.to_string(),
            snapshot_sequence: snapshot.sequence,
            capabilities,
        })
    }

    pub(super) fn retained_thread_ids(&self) -> HashSet<ThreadId> {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return HashSet::new();
        };
        snapshot
            .snapshots
            .keys()
            .chain(snapshot.overlay.loaded_thread_ids.iter())
            .copied()
            .collect()
    }

    pub(super) fn inventory_thread_ids(&self) -> HashSet<ThreadId> {
        self.snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .records
                    .iter()
                    .map(RuntimeRecord::thread_id)
                    .collect()
            })
            .unwrap_or_default()
    }
}

pub(super) fn requested_limit(limit: Option<u32>) -> Result<usize, JSONRPCErrorError> {
    match limit.map(|limit| limit as usize) {
        Some(0) => Err(invalid_params("sessionRuntime/list limit must be positive")),
        Some(limit) => Ok(limit.min(MAX_LIMIT)),
        None => Ok(DEFAULT_LIMIT),
    }
}

fn encode_cursor(cursor: &RuntimeCursor) -> Result<String, JSONRPCErrorError> {
    serde_json::to_vec(cursor)
        .map(|cursor| URL_SAFE_NO_PAD.encode(cursor))
        .map_err(|_| internal_error("session runtime cursor could not be serialized"))
}

fn decode_cursor(cursor: &str) -> Result<RuntimeCursor, JSONRPCErrorError> {
    let cursor = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| invalid_params("sessionRuntime/list cursor is invalid"))?;
    serde_json::from_slice(&cursor)
        .map_err(|_| invalid_params("sessionRuntime/list cursor is invalid"))
}

pub(super) fn stale_cursor() -> JSONRPCErrorError {
    invalid_params("sessionRuntime/list cursor is stale; restart pagination")
}
