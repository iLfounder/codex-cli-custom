use anyhow::Context;
use codex_protocol::ThreadId;
use sqlx::Row;
use sqlx::Sqlite;
use sqlx::sqlite::SqliteRow;
use std::error::Error;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadTransitionReason {
    Clear,
    New,
}

impl ThreadTransitionReason {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::New => "new",
        }
    }

    fn from_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "clear" => Ok(Self::Clear),
            "new" => Ok(Self::New),
            value => anyhow::bail!("invalid persisted thread transition reason {value}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadWriterEvidence {
    pub store_id: String,
    pub writer_generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadTransitionIntent {
    pub transition_id: String,
    pub request_fingerprint: String,
    pub reason: ThreadTransitionReason,
    pub previous_thread_id: ThreadId,
    pub previous_precondition_state_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadTransitionPreparing {
    pub transition_id: String,
    pub request_fingerprint: String,
    pub reason: ThreadTransitionReason,
    pub previous_thread_id: ThreadId,
    pub current_thread_id: ThreadId,
    pub origin_instance_epoch: String,
    pub initiator_client_incarnation: String,
    pub previous_precondition_state_revision: u64,
    pub previous_writer: ThreadWriterEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadTransitionPreparation {
    pub preparing: ThreadTransitionPreparing,
    pub current_writer: ThreadWriterEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadTransitionEndpointEvidence {
    pub thread_id: ThreadId,
    pub state_revision: u64,
    pub writer: ThreadWriterEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadTransitionReceipt {
    pub transition_id: String,
    pub reason: ThreadTransitionReason,
    pub previous: ThreadTransitionEndpointEvidence,
    pub current: ThreadTransitionEndpointEvidence,
    pub origin_instance_epoch: String,
    pub initiator_client_incarnation: String,
    pub transition_revision: u64,
    pub committed_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadTransitionRecord {
    Preparing(ThreadTransitionPreparing),
    Prepared(ThreadTransitionPreparation),
    Committed(ThreadTransitionReceipt),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadTransitionClaimOutcome {
    NewPreparing(ThreadTransitionPreparing),
    ExistingPreparing(ThreadTransitionPreparing),
    ExistingPrepared(ThreadTransitionPreparation),
    ExistingCommitted(ThreadTransitionReceipt),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarkThreadTransitionPrepared {
    pub transition_id: String,
    pub expected_request_fingerprint: String,
    pub expected_origin_instance_epoch: String,
    pub expected_initiator_client_incarnation: String,
    pub current_writer: ThreadWriterEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AbortThreadTransition {
    pub transition_id: String,
    pub expected_request_fingerprint: String,
    pub expected_origin_instance_epoch: String,
    pub expected_initiator_client_incarnation: String,
    pub expected_previous_thread_id: ThreadId,
    pub expected_current_thread_id: ThreadId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadTransitionAbortOutcome {
    Aborted,
    AlreadyAbsent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitThreadTransition {
    pub transition_id: String,
    pub expected_previous_thread_id: ThreadId,
    pub expected_current_thread_id: ThreadId,
    pub expected_origin_instance_epoch: String,
    pub expected_initiator_client_incarnation: String,
    pub previous_committed_state_revision: u64,
    pub current_committed_state_revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadTransitionCommitOutcome {
    Committed(ThreadTransitionReceipt),
    ExistingCommitted(ThreadTransitionReceipt),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommittedThreadTransitions {
    pub last_incoming: Option<ThreadTransitionReceipt>,
    pub last_outgoing: Option<ThreadTransitionReceipt>,
}

#[derive(Debug)]
pub struct ThreadTransitionConflict {
    reason: &'static str,
}

impl ThreadTransitionConflict {
    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

impl fmt::Display for ThreadTransitionConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.reason)
    }
}

impl Error for ThreadTransitionConflict {}

pub(super) fn conflict(reason: &'static str) -> anyhow::Error {
    ThreadTransitionConflict { reason }.into()
}

#[derive(Clone)]
pub(super) struct TransitionRow {
    pub revision: u64,
    pub transition_id: String,
    pub request_fingerprint: String,
    pub reason: ThreadTransitionReason,
    pub previous_thread_id: ThreadId,
    pub current_thread_id: ThreadId,
    pub origin_instance_epoch: String,
    pub initiator_client_incarnation: String,
    pub previous_precondition_state_revision: u64,
    pub previous_committed_state_revision: Option<u64>,
    pub previous_writer: ThreadWriterEvidence,
    pub current_writer: Option<ThreadWriterEvidence>,
    pub current_committed_state_revision: Option<u64>,
    pub status: String,
    pub committed_at: Option<i64>,
}

pub(super) async fn transition_row_by_id<'e, E>(
    executor: E,
    transition_id: &str,
) -> anyhow::Result<Option<TransitionRow>>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    sqlx::query("SELECT * FROM thread_transitions WHERE transition_id = ?")
        .bind(transition_id)
        .fetch_optional(executor)
        .await?
        .map(transition_row)
        .transpose()
}

pub(super) fn transition_row(row: SqliteRow) -> anyhow::Result<TransitionRow> {
    let previous_thread_id = row.try_get::<String, _>("previous_thread_id")?;
    let current_thread_id = row.try_get::<String, _>("current_thread_id")?;
    let current_writer_store_id = row.try_get::<Option<String>, _>("current_writer_store_id")?;
    let current_writer_generation = row.try_get::<Option<i64>, _>("current_writer_generation")?;
    let current_writer = match (current_writer_store_id, current_writer_generation) {
        (Some(store_id), Some(generation)) => Some(ThreadWriterEvidence {
            store_id,
            writer_generation: u64::try_from(generation)?,
        }),
        (None, None) => None,
        _ => anyhow::bail!("persisted thread transition has partial current writer evidence"),
    };
    Ok(TransitionRow {
        revision: u64::try_from(row.try_get::<i64, _>("revision")?)?,
        transition_id: row.try_get("transition_id")?,
        request_fingerprint: row.try_get("request_fingerprint")?,
        reason: ThreadTransitionReason::from_str(row.try_get("reason")?)?,
        previous_thread_id: ThreadId::from_string(&previous_thread_id)
            .with_context(|| format!("invalid previous thread id {previous_thread_id}"))?,
        current_thread_id: ThreadId::from_string(&current_thread_id)
            .with_context(|| format!("invalid current thread id {current_thread_id}"))?,
        origin_instance_epoch: row.try_get("origin_instance_epoch")?,
        initiator_client_incarnation: row.try_get("initiator_client_incarnation")?,
        previous_precondition_state_revision: u64::try_from(
            row.try_get::<i64, _>("previous_precondition_state_revision")?,
        )?,
        previous_committed_state_revision: row
            .try_get::<Option<i64>, _>("previous_committed_state_revision")?
            .map(u64::try_from)
            .transpose()?,
        previous_writer: ThreadWriterEvidence {
            store_id: row.try_get("previous_writer_store_id")?,
            writer_generation: u64::try_from(row.try_get::<i64, _>("previous_writer_generation")?)?,
        },
        current_writer,
        current_committed_state_revision: row
            .try_get::<Option<i64>, _>("current_committed_state_revision")?
            .map(u64::try_from)
            .transpose()?,
        status: row.try_get("status")?,
        committed_at: row.try_get("committed_at")?,
    })
}

pub(super) fn transition_record(row: &TransitionRow) -> anyhow::Result<ThreadTransitionRecord> {
    match row.status.as_str() {
        "preparing" => Ok(ThreadTransitionRecord::Preparing(preparing_from_row(row))),
        "prepared" => Ok(ThreadTransitionRecord::Prepared(preparation_from_row(row)?)),
        "committed" => Ok(ThreadTransitionRecord::Committed(receipt_from_row(row)?)),
        status => anyhow::bail!("invalid persisted thread transition status {status}"),
    }
}

fn preparing_from_row(row: &TransitionRow) -> ThreadTransitionPreparing {
    ThreadTransitionPreparing {
        transition_id: row.transition_id.clone(),
        request_fingerprint: row.request_fingerprint.clone(),
        reason: row.reason,
        previous_thread_id: row.previous_thread_id,
        current_thread_id: row.current_thread_id,
        origin_instance_epoch: row.origin_instance_epoch.clone(),
        initiator_client_incarnation: row.initiator_client_incarnation.clone(),
        previous_precondition_state_revision: row.previous_precondition_state_revision,
        previous_writer: row.previous_writer.clone(),
    }
}

fn preparation_from_row(row: &TransitionRow) -> anyhow::Result<ThreadTransitionPreparation> {
    Ok(ThreadTransitionPreparation {
        preparing: preparing_from_row(row),
        current_writer: row
            .current_writer
            .clone()
            .context("prepared thread transition has no current writer")?,
    })
}

pub(super) fn receipt_from_row(row: &TransitionRow) -> anyhow::Result<ThreadTransitionReceipt> {
    Ok(ThreadTransitionReceipt {
        transition_id: row.transition_id.clone(),
        reason: row.reason,
        previous: ThreadTransitionEndpointEvidence {
            thread_id: row.previous_thread_id,
            state_revision: row
                .previous_committed_state_revision
                .context("committed thread transition has no previous revision")?,
            writer: row.previous_writer.clone(),
        },
        current: ThreadTransitionEndpointEvidence {
            thread_id: row.current_thread_id,
            state_revision: row
                .current_committed_state_revision
                .context("committed thread transition has no current revision")?,
            writer: row
                .current_writer
                .clone()
                .context("committed thread transition has no current writer")?,
        },
        origin_instance_epoch: row.origin_instance_epoch.clone(),
        initiator_client_incarnation: row.initiator_client_incarnation.clone(),
        transition_revision: row.revision,
        committed_at: row
            .committed_at
            .context("committed thread transition has no commit timestamp")?,
    })
}

pub(super) fn validate_positive(value: u64, name: &str) -> anyhow::Result<()> {
    if value == 0 {
        anyhow::bail!("thread transition {name} must be positive");
    }
    let _ = i64::try_from(value)?;
    Ok(())
}

pub(super) fn validate_writer(writer: &ThreadWriterEvidence) -> anyhow::Result<()> {
    if writer.store_id.is_empty() {
        anyhow::bail!("thread transition writer store id must not be empty");
    }
    validate_positive(writer.writer_generation, "writer generation")
}
