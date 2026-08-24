use super::GitInfo;
use super::ThreadSettings;
use super::ThreadTransitionReceipt;
use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;

/// Parameters for reading a revision-consistent page of session runtime snapshots.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeListParams {
    /// Opaque cursor returned by a previous call. The server rejects it when the
    /// instance epoch or snapshot sequence no longer matches the source snapshot.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Optional page size; the server applies a bounded default and maximum.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
    /// Optional exact thread identifier filter.
    #[ts(optional = nullable)]
    pub thread_id: Option<String>,
}

/// A revision-consistent page of complete session runtime snapshots.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeListResponse {
    pub data: Vec<SessionRuntimeSnapshot>,
    /// Opaque cursor bound to `instanceEpoch`, `snapshotSequence`, and a stable
    /// sort anchor. A stale-cursor error requires restarting from the first page.
    pub next_cursor: Option<String>,
    /// Opaque identifier for this app-server process incarnation.
    pub instance_epoch: String,
    /// Event sequence captured for every page in this snapshot.
    #[ts(type = "number")]
    pub snapshot_sequence: u64,
    /// Process capabilities that external consumers may safely rely on.
    pub capabilities: Vec<SessionRuntimeCapability>,
}

/// One named process capability and its current availability.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeCapability {
    /// Stable capability name, such as `ananke_session_identity_v1`.
    pub name: String,
    pub available: bool,
    pub deny_reason: Option<String>,
}

/// Complete sanitized runtime state for one thread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeSnapshot {
    /// Logical runtime entity key.
    pub thread_id: String,
    /// Monotonic revision used by thread control compare-and-swap requests.
    #[ts(type = "number")]
    pub state_revision: u64,
    pub identity: SessionRuntimeIdentity,
    pub lifecycle: SessionRuntimeLifecycle,
    pub writer: SessionRuntimeWriter,
    pub persistence: SessionRuntimePersistence,
    pub account: SessionRuntimeAccountBinding,
    pub actions: Vec<SessionRuntimeActionAvailability>,
    #[serde(default)]
    pub continuity: SessionRuntimeContinuity,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeContinuity {
    pub last_incoming: Option<ThreadTransitionReceipt>,
    pub last_outgoing: Option<ThreadTransitionReceipt>,
}

/// Sanitized identity and settings that remain useful outside the Codex process.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeIdentity {
    pub session_id: String,
    pub forked_from_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub name: Option<String>,
    /// Stable sanitized source name from the thread metadata.
    pub source: String,
    pub cwd: String,
    pub git_info: Option<GitInfo>,
    /// Effective next-turn settings when the thread is loaded and they are available.
    pub settings: Option<ThreadSettings>,
}

/// Current lifecycle, subscription, and activity state for a thread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeLifecycle {
    pub state: SessionRuntimeLifecycleState,
    pub active_turn_id: Option<String>,
    pub waiting_on: Vec<SessionRuntimeWaitingOn>,
    pub subscriber_count: u32,
    /// Opaque client incarnation identifiers; raw process identifiers are never exposed.
    pub client_incarnations: Vec<String>,
    /// Unix timestamp in seconds of the most recent runtime activity.
    #[ts(type = "number | null")]
    pub last_activity_at: Option<i64>,
    /// Unix timestamp in seconds when an idle unload is scheduled.
    #[ts(type = "number | null")]
    pub unload_at: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum SessionRuntimeLifecycleState {
    NotLoaded,
    Loaded,
    Idle,
    Active,
    Closing,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum SessionRuntimeWaitingOn {
    Approval,
    UserInput,
    ServerRequest,
}

/// Writer ownership and its durable lease fence.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeWriter {
    pub state: SessionRuntimeWriterState,
    /// Opaque store identity. This is not a filesystem path or process identifier.
    pub store_id: Option<String>,
    #[ts(type = "number | null")]
    pub writer_generation: Option<u64>,
    pub deny_reason: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum SessionRuntimeWriterState {
    None,
    OwnedHere,
    OwnedElsewhere,
    Unavailable,
}

/// Durable JSONL/SQLite progress and health without exposing storage paths.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimePersistence {
    pub jsonl: Option<SessionRuntimePersistencePosition>,
    pub sqlite: Option<SessionRuntimePersistencePosition>,
    /// Number of durable records by which SQLite trails JSONL, when known.
    #[ts(type = "number | null")]
    pub lag: Option<u64>,
    pub flush_health: SessionRuntimePersistenceHealth,
    pub materialize_health: SessionRuntimePersistenceHealth,
    /// Unix timestamp in seconds of the last successful recorder flush.
    #[ts(type = "number | null")]
    pub flushed_at: Option<i64>,
    /// Unix timestamp in seconds of the last successful SQLite materialization.
    #[ts(type = "number | null")]
    pub materialized_at: Option<i64>,
    pub deny_reason: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimePersistencePosition {
    #[ts(type = "number")]
    pub ordinal: u64,
    #[ts(type = "number")]
    pub offset: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum SessionRuntimePersistenceHealth {
    Unknown,
    Healthy,
    Degraded,
    Failed,
}

/// Current and active-turn account provenance for a thread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeAccountBinding {
    pub current: Option<SessionRuntimeAccountRef>,
    pub active_turn: Option<SessionRuntimeAccountRef>,
    pub switch_state: SessionRuntimeAccountSwitchState,
    pub switch_target_slot_id: Option<String>,
    pub deny_reason: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeAccountRef {
    pub account_slot_id: String,
    #[ts(type = "number")]
    pub execution_generation: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum SessionRuntimeAccountSwitchState {
    Stable,
    Preparing,
    Switching,
    Unbound,
    Degraded,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum SessionRuntimeAction {
    Relinquish,
    SwitchAccount,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeActionAvailability {
    pub action: SessionRuntimeAction,
    pub allowed: bool,
    pub deny_reason: Option<String>,
}

/// Common operation state returned by controls and published as progress changes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeOperation {
    pub operation_id: String,
    /// Opaque digest of the normalized action and arguments. Reusing an operation id
    /// with a different fingerprint is an invalid request rather than an idempotent retry.
    pub request_fingerprint: String,
    pub action: SessionRuntimeOperationAction,
    pub status: SessionRuntimeOperationStatus,
    pub thread_id: Option<String>,
    pub account_slot_id: Option<String>,
    #[ts(type = "number | null")]
    pub state_revision: Option<u64>,
    #[ts(type = "number | null")]
    pub writer_generation: Option<u64>,
    #[ts(type = "number | null")]
    pub execution_generation: Option<u64>,
    pub error: Option<SessionRuntimeOperationError>,
    /// Unix timestamp in seconds when this operation state was produced.
    #[ts(type = "number")]
    pub updated_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum SessionRuntimeOperationAction {
    AccountSlotLogin,
    ThreadAccountSwitch,
    ThreadRelinquish,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum SessionRuntimeOperationStatus {
    Accepted,
    Running,
    Released,
    Ready,
    Failed,
}

/// Sanitized operation failure; it never contains tokens, paths, sockets, or raw PIDs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeOperationError {
    pub code: String,
    pub message: String,
}

/// Compare-and-swap parameters for switching a thread's next-turn account.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAccountSwitchParams {
    pub operation_id: String,
    pub thread_id: String,
    pub target_account_slot_id: String,
    pub expected_instance_epoch: String,
    #[ts(type = "number")]
    pub expected_state_revision: u64,
    #[ts(type = "number")]
    pub expected_execution_generation: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadAccountSwitchResponse {
    pub operation: SessionRuntimeOperation,
}

/// Compare-and-swap parameters for a strict durable writer release.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRelinquishParams {
    pub operation_id: String,
    pub thread_id: String,
    pub expected_instance_epoch: String,
    #[ts(type = "number")]
    pub expected_state_revision: u64,
    #[ts(type = "number")]
    pub expected_writer_generation: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ThreadRelinquishResponse {
    pub operation: SessionRuntimeOperation,
}

/// Full changed-thread snapshot on the process runtime event stream.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeChangedNotification {
    pub instance_epoch: String,
    #[ts(type = "number")]
    pub sequence: u64,
    pub snapshot: SessionRuntimeSnapshot,
}

/// Progress or terminal state for a bounded process-local operation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct SessionRuntimeOperationUpdatedNotification {
    pub instance_epoch: String,
    #[ts(type = "number")]
    pub sequence: u64,
    pub operation: SessionRuntimeOperation,
}
