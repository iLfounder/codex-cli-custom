use super::goals::GoalClearOutcome;
use super::goals::GoalReplaceOutcome;
use super::goals::GoalStore;
use super::goals::GoalUpdate;
use super::goals::GoalVersion;
use super::goals::status_after_budget_limit;
use super::goals::thread_goal_from_row;
use super::*;
use uuid::Uuid;

impl GoalStore {
    pub async fn create_thread_goal_exact(
        &self,
        thread_id: ThreadId,
        expected_revision: i64,
        objective: &str,
        status: crate::ThreadGoalStatus,
        token_budget: Option<i64>,
    ) -> anyhow::Result<Option<crate::ThreadGoal>> {
        if expected_revision < 0 || expected_revision == i64::MAX {
            return Ok(None);
        }
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (current_revision, has_goal): (i64, bool) = sqlx::query_as(
            r#"
SELECT COALESCE(
    (SELECT revision FROM thread_goals WHERE thread_id = ?),
    (SELECT revision FROM thread_goal_revision_tombstones WHERE thread_id = ?),
    0
), EXISTS(SELECT 1 FROM thread_goals WHERE thread_id = ?)
            "#,
        )
        .bind(thread_id.to_string())
        .bind(thread_id.to_string())
        .bind(thread_id.to_string())
        .fetch_one(&mut *transaction)
        .await?;
        if has_goal || current_revision != expected_revision {
            transaction.rollback().await?;
            return Ok(None);
        }
        let revision = expected_revision + 1;
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let status = status_after_budget_limit(status, /*tokens_used*/ 0, token_budget);
        let row = sqlx::query(
            r#"
INSERT INTO thread_goals (
    thread_id, goal_id, revision, objective, status, token_budget,
    tokens_used, time_used_seconds, created_at_ms, updated_at_ms
) VALUES (?, ?, ?, ?, ?, ?, 0, 0, ?, ?)
RETURNING
    thread_id, goal_id, revision, objective, status, token_budget,
    tokens_used, time_used_seconds, created_at_ms, updated_at_ms
            "#,
        )
        .bind(thread_id.to_string())
        .bind(Uuid::new_v4().to_string())
        .bind(revision)
        .bind(objective)
        .bind(status.as_str())
        .bind(token_budget)
        .bind(now_ms)
        .bind(now_ms)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.rollback().await?;
            return Ok(None);
        };
        sqlx::query("DELETE FROM thread_goal_revision_tombstones WHERE thread_id = ?")
            .bind(thread_id.to_string())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        thread_goal_from_row(&row).map(Some)
    }

    pub async fn replace_thread_goal_exact(
        &self,
        thread_id: ThreadId,
        expected: &GoalVersion,
        objective: &str,
        status: crate::ThreadGoalStatus,
        token_budget: Option<i64>,
    ) -> anyhow::Result<Option<GoalReplaceOutcome>> {
        if expected.revision == i64::MAX {
            return Ok(None);
        }
        let Some(previous_goal) = self.get_thread_goal(thread_id).await? else {
            return Ok(None);
        };
        if GoalVersion::from(&previous_goal) != *expected {
            return Ok(None);
        }
        let revision = expected.revision + 1;
        let now_ms = datetime_to_epoch_millis(Utc::now());
        let status = status_after_budget_limit(status, /*tokens_used*/ 0, token_budget);
        let row = sqlx::query(
            r#"
UPDATE thread_goals
SET goal_id = ?, revision = ?, objective = ?, status = ?, token_budget = ?,
    tokens_used = 0, time_used_seconds = 0, created_at_ms = ?, updated_at_ms = ?
WHERE thread_id = ? AND goal_id = ? AND revision = ?
RETURNING
    thread_id, goal_id, revision, objective, status, token_budget,
    tokens_used, time_used_seconds, created_at_ms, updated_at_ms
            "#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(revision)
        .bind(objective)
        .bind(status.as_str())
        .bind(token_budget)
        .bind(now_ms)
        .bind(now_ms)
        .bind(thread_id.to_string())
        .bind(&expected.goal_id)
        .bind(expected.revision)
        .fetch_optional(self.pool.as_ref())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(GoalReplaceOutcome {
            previous_goal,
            goal: thread_goal_from_row(&row)?,
        }))
    }

    pub async fn clear_thread_goal_exact(
        &self,
        thread_id: ThreadId,
        expected: &GoalVersion,
    ) -> anyhow::Result<Option<GoalClearOutcome>> {
        if expected.revision == i64::MAX {
            return Ok(None);
        }
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query(
            r#"
DELETE FROM thread_goals
WHERE thread_id = ? AND goal_id = ? AND revision = ?
RETURNING
    thread_id, goal_id, revision, objective, status, token_budget,
    tokens_used, time_used_seconds, created_at_ms, updated_at_ms
            "#,
        )
        .bind(thread_id.to_string())
        .bind(&expected.goal_id)
        .bind(expected.revision)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(row) = row else {
            transaction.rollback().await?;
            return Ok(None);
        };
        let revision = expected.revision + 1;
        sqlx::query(
            r#"
INSERT INTO thread_goal_revision_tombstones (thread_id, revision)
VALUES (?, ?)
ON CONFLICT(thread_id) DO UPDATE SET revision = excluded.revision
            "#,
        )
        .bind(thread_id.to_string())
        .bind(revision)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(Some(GoalClearOutcome {
            previous_goal: thread_goal_from_row(&row)?,
            revision,
        }))
    }

    pub async fn update_thread_goal_exact(
        &self,
        thread_id: ThreadId,
        expected: &GoalVersion,
        update: GoalUpdate,
    ) -> anyhow::Result<Option<crate::ThreadGoal>> {
        let Some(current) = self.get_thread_goal(thread_id).await? else {
            return Ok(None);
        };
        if GoalVersion::from(&current) != *expected || expected.revision == i64::MAX {
            return Ok(None);
        }
        if update.objective.is_none() && update.status.is_none() && update.token_budget.is_none() {
            return Ok(Some(current));
        }
        let objective = update.objective.unwrap_or(current.objective);
        let token_budget = update.token_budget.unwrap_or(current.token_budget);
        let requested_status = update.status.unwrap_or(current.status);
        let status = if current.status == crate::ThreadGoalStatus::BudgetLimited
            && matches!(
                requested_status,
                crate::ThreadGoalStatus::Paused | crate::ThreadGoalStatus::Blocked
            ) {
            current.status
        } else {
            status_after_budget_limit(requested_status, current.tokens_used, token_budget)
        };
        let row = sqlx::query(
            r#"
UPDATE thread_goals
SET objective = ?, status = ?, token_budget = ?, revision = revision + 1, updated_at_ms = ?
WHERE thread_id = ? AND goal_id = ? AND revision = ? AND revision < 9223372036854775807
RETURNING
    thread_id, goal_id, revision, objective, status, token_budget,
    tokens_used, time_used_seconds, created_at_ms, updated_at_ms
            "#,
        )
        .bind(objective)
        .bind(status.as_str())
        .bind(token_budget)
        .bind(datetime_to_epoch_millis(Utc::now()))
        .bind(thread_id.to_string())
        .bind(&expected.goal_id)
        .bind(expected.revision)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(|row| thread_goal_from_row(&row)).transpose()
    }

    pub(crate) async fn purge_thread_goal(&self, thread_id: ThreadId) -> anyhow::Result<()> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM thread_goals WHERE thread_id = ?")
            .bind(thread_id.to_string())
            .execute(&mut *transaction)
            .await?;
        sqlx::query("DELETE FROM thread_goal_revision_tombstones WHERE thread_id = ?")
            .bind(thread_id.to_string())
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn account_thread_goal_usage_exact(
        &self,
        thread_id: ThreadId,
        time_delta_seconds: i64,
        token_delta: i64,
        mode: super::goals::GoalAccountingMode,
        expected: &GoalVersion,
    ) -> anyhow::Result<super::goals::GoalAccountingOutcome> {
        self.account_thread_goal_usage_inner(
            thread_id,
            time_delta_seconds,
            token_delta,
            mode,
            Some(expected.goal_id.as_str()),
            Some(expected.revision),
        )
        .await
    }
}
