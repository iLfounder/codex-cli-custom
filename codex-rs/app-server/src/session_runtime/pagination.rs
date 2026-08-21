use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::SessionRuntimeCapability;
use codex_app_server_protocol::SessionRuntimeListResponse;
use codex_app_server_protocol::SessionRuntimeSnapshot;
use serde::Deserialize;
use serde::Serialize;

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
    snapshots: Vec<SessionRuntimeSnapshot>,
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
    pub(super) fn insert(&mut self, snapshot: CachedSnapshot) {
        self.snapshot = Some(snapshot);
    }

    pub(super) fn clear(&mut self) {
        self.snapshot = None;
    }

    pub(super) fn first_page(
        &self,
        instance_epoch: &str,
        current_sequence: u64,
        limit: usize,
        capabilities: Vec<SessionRuntimeCapability>,
    ) -> Result<Option<SessionRuntimeListResponse>, JSONRPCErrorError> {
        let Some(snapshot) = self
            .snapshot
            .as_ref()
            .filter(|snapshot| snapshot.sequence == current_sequence)
        else {
            return Ok(None);
        };
        snapshot
            .first_page(instance_epoch, limit, capabilities)
            .map(Some)
    }

    pub(super) fn page(
        &self,
        encoded_cursor: &str,
        instance_epoch: &str,
        current_sequence: u64,
        limit: usize,
        capabilities: Vec<SessionRuntimeCapability>,
    ) -> Result<SessionRuntimeListResponse, JSONRPCErrorError> {
        let cursor = decode_cursor(encoded_cursor)?;
        if cursor.instance_epoch != instance_epoch || cursor.snapshot_sequence != current_sequence {
            return Err(stale_cursor());
        }
        let snapshot = self.snapshot.as_ref().ok_or_else(stale_cursor)?;
        if snapshot.id != cursor.snapshot_id || snapshot.sequence != cursor.snapshot_sequence {
            return Err(stale_cursor());
        }
        let start = snapshot
            .snapshots
            .iter()
            .position(|item| item.thread_id == cursor.after_thread_id)
            .map(|index| index + 1)
            .ok_or_else(|| invalid_params("sessionRuntime/list cursor is invalid"))?;
        snapshot.page(instance_epoch, start, limit, capabilities)
    }
}

impl CachedSnapshot {
    pub(super) fn new(sequence: u64, snapshots: Vec<SessionRuntimeSnapshot>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            sequence,
            snapshots,
        }
    }

    pub(super) fn first_page(
        &self,
        instance_epoch: &str,
        limit: usize,
        capabilities: Vec<SessionRuntimeCapability>,
    ) -> Result<SessionRuntimeListResponse, JSONRPCErrorError> {
        self.page(instance_epoch, /*start*/ 0, limit, capabilities)
    }

    fn page(
        &self,
        instance_epoch: &str,
        start: usize,
        limit: usize,
        capabilities: Vec<SessionRuntimeCapability>,
    ) -> Result<SessionRuntimeListResponse, JSONRPCErrorError> {
        let end = start.saturating_add(limit).min(self.snapshots.len());
        let data = self.snapshots[start..end].to_vec();
        let next_cursor = if end == 0 || end >= self.snapshots.len() {
            None
        } else {
            Some(encode_cursor(&RuntimeCursor {
                instance_epoch: instance_epoch.to_string(),
                snapshot_sequence: self.sequence,
                snapshot_id: self.id.clone(),
                after_thread_id: self.snapshots[end - 1].thread_id.clone(),
            })?)
        };
        Ok(SessionRuntimeListResponse {
            data,
            next_cursor,
            instance_epoch: instance_epoch.to_string(),
            snapshot_sequence: self.sequence,
            capabilities,
        })
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

fn stale_cursor() -> JSONRPCErrorError {
    invalid_params("sessionRuntime/list cursor is stale; restart pagination")
}
