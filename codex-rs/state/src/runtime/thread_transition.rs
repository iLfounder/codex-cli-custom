use anyhow::Context;
use codex_protocol::ThreadId;
use sqlx::QueryBuilder;
use sqlx::Sqlite;
use std::collections::HashMap;

use super::StateRuntime;
mod types;
pub use types::*;

impl StateRuntime {
    pub async fn claim_thread_transition(
        &self,
        intent: &ThreadTransitionIntent,
        reserved_current_thread_id: ThreadId,
        origin_instance_epoch: &str,
        initiator_client_incarnation: &str,
        previous_writer: &ThreadWriterEvidence,
    ) -> anyhow::Result<ThreadTransitionClaimOutcome> {
        validate_positive(
            intent.previous_precondition_state_revision,
            "state revision",
        )?;
        validate_writer(previous_writer)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(row) = transition_row_by_id(&mut *transaction, &intent.transition_id).await? {
            let record = transition_record(&row)?;
            if row.request_fingerprint != intent.request_fingerprint
                || row.reason != intent.reason
                || row.previous_thread_id != intent.previous_thread_id
                || row.previous_precondition_state_revision
                    != intent.previous_precondition_state_revision
                || row.previous_writer != *previous_writer
            {
                transaction.rollback().await?;
                return Err(conflict("transition_id_conflict"));
            }
            if !matches!(record, ThreadTransitionRecord::Committed(_))
                && (row.origin_instance_epoch != origin_instance_epoch
                    || row.initiator_client_incarnation != initiator_client_incarnation)
            {
                transaction.rollback().await?;
                return Err(conflict("transition_initiator_mismatch"));
            }
            transaction.commit().await?;
            return Ok(match record {
                ThreadTransitionRecord::Preparing(value) => {
                    ThreadTransitionClaimOutcome::ExistingPreparing(value)
                }
                ThreadTransitionRecord::Prepared(value) => {
                    ThreadTransitionClaimOutcome::ExistingPrepared(value)
                }
                ThreadTransitionRecord::Committed(value) => {
                    ThreadTransitionClaimOutcome::ExistingCommitted(value)
                }
            });
        }

        let insert = sqlx::query(
            "INSERT INTO thread_transitions (transition_id, request_fingerprint, reason, \
             previous_thread_id, current_thread_id, origin_instance_epoch, \
             initiator_client_incarnation, previous_precondition_state_revision, \
             previous_writer_store_id, previous_writer_generation, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'preparing')",
        )
        .bind(&intent.transition_id)
        .bind(&intent.request_fingerprint)
        .bind(intent.reason.as_str())
        .bind(intent.previous_thread_id.to_string())
        .bind(reserved_current_thread_id.to_string())
        .bind(origin_instance_epoch)
        .bind(initiator_client_incarnation)
        .bind(i64::try_from(intent.previous_precondition_state_revision)?)
        .bind(&previous_writer.store_id)
        .bind(i64::try_from(previous_writer.writer_generation)?)
        .execute(&mut *transaction)
        .await;
        if let Err(error) = insert {
            transaction.rollback().await?;
            if error
                .as_database_error()
                .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
            {
                return Err(conflict("transition_thread_mismatch"));
            }
            return Err(error.into());
        }
        let row = transition_row_by_id(&mut *transaction, &intent.transition_id)
            .await?
            .context("inserted thread transition disappeared")?;
        transaction.commit().await?;
        let ThreadTransitionRecord::Preparing(preparing) = transition_record(&row)? else {
            anyhow::bail!("new thread transition did not persist as preparing");
        };
        Ok(ThreadTransitionClaimOutcome::NewPreparing(preparing))
    }

    pub async fn mark_thread_transition_prepared(
        &self,
        request: &MarkThreadTransitionPrepared,
    ) -> anyhow::Result<ThreadTransitionPreparation> {
        validate_writer(&request.current_writer)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = transition_row_by_id(&mut *transaction, &request.transition_id)
            .await?
            .ok_or_else(|| conflict("transition_not_prepared"))?;
        if row.request_fingerprint != request.expected_request_fingerprint {
            transaction.rollback().await?;
            return Err(conflict("transition_id_conflict"));
        }
        if row.origin_instance_epoch != request.expected_origin_instance_epoch
            || row.initiator_client_incarnation != request.expected_initiator_client_incarnation
        {
            transaction.rollback().await?;
            return Err(conflict("transition_initiator_mismatch"));
        }
        match transition_record(&row)? {
            ThreadTransitionRecord::Preparing(_) => {}
            ThreadTransitionRecord::Prepared(preparation)
                if preparation.current_writer == request.current_writer =>
            {
                transaction.commit().await?;
                return Ok(preparation);
            }
            ThreadTransitionRecord::Prepared(_) => {
                transaction.rollback().await?;
                return Err(conflict("stale_writer_fence"));
            }
            ThreadTransitionRecord::Committed(_) => {
                transaction.rollback().await?;
                return Err(conflict("transition_not_prepared"));
            }
        }
        let updated = sqlx::query(
            "UPDATE thread_transitions SET current_writer_store_id = ?, \
             current_writer_generation = ?, status = 'prepared' \
             WHERE transition_id = ? AND status = 'preparing' \
             AND request_fingerprint = ? AND origin_instance_epoch = ? \
             AND initiator_client_incarnation = ?",
        )
        .bind(&request.current_writer.store_id)
        .bind(i64::try_from(request.current_writer.writer_generation)?)
        .bind(&request.transition_id)
        .bind(&request.expected_request_fingerprint)
        .bind(&request.expected_origin_instance_epoch)
        .bind(&request.expected_initiator_client_incarnation)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(conflict("transition_not_prepared"));
        }
        let row = transition_row_by_id(&mut *transaction, &request.transition_id)
            .await?
            .context("prepared thread transition disappeared")?;
        transaction.commit().await?;
        let ThreadTransitionRecord::Prepared(preparation) = transition_record(&row)? else {
            anyhow::bail!("thread transition did not persist as prepared");
        };
        Ok(preparation)
    }

    pub async fn commit_thread_transition(
        &self,
        request: &CommitThreadTransition,
    ) -> anyhow::Result<ThreadTransitionCommitOutcome> {
        validate_positive(request.previous_committed_state_revision, "state revision")?;
        validate_positive(request.current_committed_state_revision, "state revision")?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = transition_row_by_id(&mut *transaction, &request.transition_id)
            .await?
            .ok_or_else(|| conflict("transition_not_prepared"))?;
        if row.previous_thread_id != request.expected_previous_thread_id
            || row.current_thread_id != request.expected_current_thread_id
        {
            transaction.rollback().await?;
            return Err(conflict("transition_thread_mismatch"));
        }
        if row.origin_instance_epoch != request.expected_origin_instance_epoch
            || row.initiator_client_incarnation != request.expected_initiator_client_incarnation
        {
            transaction.rollback().await?;
            return Err(conflict("transition_initiator_mismatch"));
        }
        match transition_record(&row)? {
            ThreadTransitionRecord::Committed(receipt) => {
                transaction.commit().await?;
                return Ok(ThreadTransitionCommitOutcome::ExistingCommitted(receipt));
            }
            ThreadTransitionRecord::Preparing(_) => {
                transaction.rollback().await?;
                return Err(conflict("transition_not_prepared"));
            }
            ThreadTransitionRecord::Prepared(_) => {}
        }
        let outgoing_exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM thread_transitions WHERE status = 'committed' \
             AND previous_thread_id = ? AND previous_precondition_state_revision = ? \
             AND previous_writer_store_id = ? AND previous_writer_generation = ? \
             AND transition_id <> ?)",
        )
        .bind(row.previous_thread_id.to_string())
        .bind(i64::try_from(row.previous_precondition_state_revision)?)
        .bind(&row.previous_writer.store_id)
        .bind(i64::try_from(row.previous_writer.writer_generation)?)
        .bind(&request.transition_id)
        .fetch_one(&mut *transaction)
        .await?;
        if outgoing_exists {
            transaction.rollback().await?;
            return Err(conflict("outgoing_transition_conflict"));
        }
        let updated = sqlx::query(
            "UPDATE thread_transitions SET previous_committed_state_revision = ?, \
             current_committed_state_revision = ?, status = 'committed', committed_at = unixepoch() \
             WHERE transition_id = ? AND status = 'prepared' AND previous_thread_id = ? \
             AND current_thread_id = ? AND origin_instance_epoch = ? \
             AND initiator_client_incarnation = ?",
        )
        .bind(i64::try_from(request.previous_committed_state_revision)?)
        .bind(i64::try_from(request.current_committed_state_revision)?)
        .bind(&request.transition_id)
        .bind(request.expected_previous_thread_id.to_string())
        .bind(request.expected_current_thread_id.to_string())
        .bind(&request.expected_origin_instance_epoch)
        .bind(&request.expected_initiator_client_incarnation)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(conflict("transition_not_prepared"));
        }
        let row = transition_row_by_id(&mut *transaction, &request.transition_id)
            .await?
            .context("committed thread transition disappeared")?;
        transaction.commit().await?;
        let ThreadTransitionRecord::Committed(receipt) = transition_record(&row)? else {
            anyhow::bail!("thread transition did not persist as committed");
        };
        Ok(ThreadTransitionCommitOutcome::Committed(receipt))
    }

    pub async fn abort_thread_transition(
        &self,
        request: &AbortThreadTransition,
    ) -> anyhow::Result<ThreadTransitionAbortOutcome> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let Some(row) = transition_row_by_id(&mut *transaction, &request.transition_id).await?
        else {
            transaction.commit().await?;
            return Ok(ThreadTransitionAbortOutcome::AlreadyAbsent);
        };
        if row.request_fingerprint != request.expected_request_fingerprint
            || row.origin_instance_epoch != request.expected_origin_instance_epoch
            || row.initiator_client_incarnation != request.expected_initiator_client_incarnation
            || row.previous_thread_id != request.expected_previous_thread_id
            || row.current_thread_id != request.expected_current_thread_id
        {
            transaction.rollback().await?;
            return Err(conflict("transition_abort_mismatch"));
        }
        if matches!(
            transition_record(&row)?,
            ThreadTransitionRecord::Committed(_)
        ) {
            transaction.rollback().await?;
            return Err(conflict("transition_already_committed"));
        }
        let deleted = sqlx::query(
            "DELETE FROM thread_transitions WHERE transition_id = ? \
             AND request_fingerprint = ? AND origin_instance_epoch = ? \
             AND initiator_client_incarnation = ? AND previous_thread_id = ? \
             AND current_thread_id = ? AND status IN ('preparing', 'prepared')",
        )
        .bind(&request.transition_id)
        .bind(&request.expected_request_fingerprint)
        .bind(&request.expected_origin_instance_epoch)
        .bind(&request.expected_initiator_client_incarnation)
        .bind(request.expected_previous_thread_id.to_string())
        .bind(request.expected_current_thread_id.to_string())
        .execute(&mut *transaction)
        .await?;
        if deleted.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(conflict("transition_abort_mismatch"));
        }
        transaction.commit().await?;
        Ok(ThreadTransitionAbortOutcome::Aborted)
    }

    pub async fn thread_transition_by_id(
        &self,
        transition_id: &str,
    ) -> anyhow::Result<Option<ThreadTransitionRecord>> {
        let mut connection = self.pool.acquire().await?;
        transition_row_by_id(&mut *connection, transition_id)
            .await?
            .as_ref()
            .map(transition_record)
            .transpose()
    }

    pub async fn committed_thread_transitions_for_threads(
        &self,
        thread_ids: &[ThreadId],
    ) -> anyhow::Result<HashMap<ThreadId, CommittedThreadTransitions>> {
        let mut result = thread_ids
            .iter()
            .copied()
            .map(|thread_id| (thread_id, CommittedThreadTransitions::default()))
            .collect::<HashMap<_, _>>();
        if result.is_empty() {
            return Ok(result);
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT * FROM thread_transitions WHERE status = 'committed' AND (previous_thread_id IN (",
        );
        let mut separated = query.separated(", ");
        for thread_id in result.keys() {
            separated.push_bind(thread_id.to_string());
        }
        separated.push_unseparated(") OR current_thread_id IN (");
        let mut separated = query.separated(", ");
        for thread_id in result.keys() {
            separated.push_bind(thread_id.to_string());
        }
        separated.push_unseparated(")) ORDER BY revision DESC");
        for row in query.build().fetch_all(self.pool.as_ref()).await? {
            let receipt = receipt_from_row(&transition_row(row)?)?;
            if let Some(continuity) = result.get_mut(&receipt.previous.thread_id)
                && continuity.last_outgoing.is_none()
            {
                continuity.last_outgoing = Some(receipt.clone());
            }
            if let Some(continuity) = result.get_mut(&receipt.current.thread_id)
                && continuity.last_incoming.is_none()
            {
                continuity.last_incoming = Some(receipt);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
#[path = "thread_transition_tests.rs"]
mod tests;
