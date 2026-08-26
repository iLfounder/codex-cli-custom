use std::collections::HashSet;

use anyhow::Context;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::StateRuntime;

const DEFAULT_ACCOUNT_SLOT_ID: &str = "default";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadAccountRotationMode {
    Fixed,
    QuotaAware,
    RoundRobin,
    ExhaustThenNext,
}

impl ThreadAccountRotationMode {
    fn as_db_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::QuotaAware => "quotaAware",
            Self::RoundRobin => "roundRobin",
            Self::ExhaustThenNext => "exhaustThenNext",
        }
    }

    fn from_db_str(value: &str) -> anyhow::Result<Self> {
        match value {
            "fixed" => Ok(Self::Fixed),
            "quotaAware" => Ok(Self::QuotaAware),
            "roundRobin" => Ok(Self::RoundRobin),
            "exhaustThenNext" => Ok(Self::ExhaustThenNext),
            _ => anyhow::bail!("invalid thread account rotation mode {value}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadAccountRotationPolicy {
    pub mode: ThreadAccountRotationMode,
    pub fixed_account_slot_id: Option<String>,
    pub automatic_account_slot_ids: Vec<String>,
    pub revision: u64,
    pub last_committed_account_slot_id: Option<String>,
}

impl ThreadAccountRotationPolicy {
    pub fn virtual_fixed(binding: &ExecutionAccountBinding) -> Self {
        Self {
            mode: ThreadAccountRotationMode::Fixed,
            fixed_account_slot_id: Some(binding.slot_id.clone()),
            automatic_account_slot_ids: Vec::new(),
            revision: 0,
            last_committed_account_slot_id: Some(binding.slot_id.clone()),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadAccountRotationPolicyUpdate {
    pub mode: ThreadAccountRotationMode,
    pub fixed_account_slot_id: Option<String>,
    pub automatic_account_slot_ids: Vec<String>,
}

impl ThreadAccountRotationPolicyUpdate {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.mode != ThreadAccountRotationMode::Fixed
            && self.automatic_account_slot_ids.is_empty()
        {
            anyhow::bail!("automatic account rotation requires at least one account slot");
        }
        self.validate_persisted()
    }

    fn validate_persisted(&self) -> anyhow::Result<()> {
        if self.mode == ThreadAccountRotationMode::Fixed && self.fixed_account_slot_id.is_none() {
            anyhow::bail!("fixed account rotation requires a fixed account slot");
        }
        let mut unique = HashSet::with_capacity(self.automatic_account_slot_ids.len());
        if self
            .automatic_account_slot_ids
            .iter()
            .any(|slot_id| slot_id.is_empty() || !unique.insert(slot_id))
        {
            anyhow::bail!("automatic account rotation slots must be non-empty and distinct");
        }
        if self.fixed_account_slot_id.as_deref() == Some("") {
            anyhow::bail!("fixed account rotation slot must not be empty");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountBindingCommitIntent {
    PreserveRotation,
    PinFixed,
}

impl StateRuntime {
    pub async fn thread_account_rotation_policy(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<ThreadAccountRotationPolicy> {
        if let Some(policy) = read_policy(self.pool.as_ref(), thread_id).await? {
            return Ok(policy);
        }
        let binding = self
            .execution_account_binding(thread_id)
            .await?
            .unwrap_or_else(default_execution_account_binding);
        Ok(ThreadAccountRotationPolicy::virtual_fixed(&binding))
    }

    pub async fn compare_and_swap_thread_account_rotation_policy(
        &self,
        thread_id: ThreadId,
        expected_revision: u64,
        update: &ThreadAccountRotationPolicyUpdate,
    ) -> anyhow::Result<Option<ThreadAccountRotationPolicy>> {
        update.validate()?;
        let next_revision = expected_revision
            .checked_add(1)
            .context("thread account rotation revision overflow")?;
        let automatic_slot_ids_json = serde_json::to_string(&update.automatic_account_slot_ids)?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = read_policy(&mut *transaction, thread_id).await?;
        let current_revision = current.as_ref().map_or(0, |policy| policy.revision);
        if current_revision != expected_revision {
            transaction.rollback().await?;
            return Ok(None);
        }

        let rows_affected = if current.is_none() {
            let last_committed_slot_id = sqlx::query_scalar::<_, String>(
                "SELECT slot_id FROM thread_execution_account_bindings WHERE thread_id = ?",
            )
            .bind(thread_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .unwrap_or_else(|| DEFAULT_ACCOUNT_SLOT_ID.to_string());
            sqlx::query(
                "INSERT INTO thread_account_rotation_policies \
                 (thread_id, revision, mode, fixed_slot_id, automatic_slot_ids_json, \
                  last_committed_slot_id, updated_at) VALUES (?, ?, ?, ?, ?, ?, unixepoch())",
            )
            .bind(thread_id.to_string())
            .bind(i64::try_from(next_revision)?)
            .bind(update.mode.as_db_str())
            .bind(&update.fixed_account_slot_id)
            .bind(&automatic_slot_ids_json)
            .bind(last_committed_slot_id)
            .execute(&mut *transaction)
            .await?
            .rows_affected()
        } else {
            sqlx::query(
                "UPDATE thread_account_rotation_policies SET revision = ?, mode = ?, \
                 fixed_slot_id = ?, automatic_slot_ids_json = ?, updated_at = unixepoch() \
                 WHERE thread_id = ? AND revision = ?",
            )
            .bind(i64::try_from(next_revision)?)
            .bind(update.mode.as_db_str())
            .bind(&update.fixed_account_slot_id)
            .bind(&automatic_slot_ids_json)
            .bind(thread_id.to_string())
            .bind(i64::try_from(expected_revision)?)
            .execute(&mut *transaction)
            .await?
            .rows_affected()
        };
        if rows_affected != 1 {
            transaction.rollback().await?;
            return Ok(None);
        }
        let committed = read_policy(&mut *transaction, thread_id)
            .await?
            .context("thread account rotation policy disappeared after commit")?;
        transaction.commit().await?;
        Ok(Some(committed))
    }

    pub async fn compare_and_swap_thread_account_rotation_cursor(
        &self,
        thread_id: ThreadId,
        expected_revision: u64,
        accepted_account_slot_id: &str,
    ) -> anyhow::Result<Option<ThreadAccountRotationPolicy>> {
        if accepted_account_slot_id.is_empty() {
            anyhow::bail!("accepted account slot must not be empty");
        }
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let updated = sqlx::query(
            "UPDATE thread_account_rotation_policies SET last_committed_slot_id = ?, \
             updated_at = unixepoch() WHERE thread_id = ? AND revision = ?",
        )
        .bind(accepted_account_slot_id)
        .bind(thread_id.to_string())
        .bind(i64::try_from(expected_revision)?)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(None);
        }
        let committed = read_policy(&mut *transaction, thread_id)
            .await?
            .context("thread account rotation policy disappeared after cursor commit")?;
        transaction.commit().await?;
        Ok(Some(committed))
    }

    pub async fn remove_account_slot_from_automatic_rotation_policies(
        &self,
        account_slot_id: &str,
    ) -> anyhow::Result<Vec<(ThreadId, ThreadAccountRotationPolicy)>> {
        if account_slot_id.is_empty() {
            anyhow::bail!("account slot must not be empty");
        }
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let rows = sqlx::query(
            "SELECT thread_id, revision, mode, fixed_slot_id, automatic_slot_ids_json, \
             last_committed_slot_id FROM thread_account_rotation_policies ORDER BY thread_id",
        )
        .fetch_all(&mut *transaction)
        .await?;
        let mut affected = Vec::new();
        for row in rows {
            let thread_id = ThreadId::from_string(row.try_get::<String, _>("thread_id")?.as_str())?;
            let mut policy = policy_from_row(&row)?;
            if !policy
                .automatic_account_slot_ids
                .iter()
                .any(|slot_id| slot_id == account_slot_id)
            {
                continue;
            }
            policy
                .automatic_account_slot_ids
                .retain(|slot_id| slot_id != account_slot_id);
            let previous_revision = i64::try_from(policy.revision)?;
            policy.revision = policy
                .revision
                .checked_add(1)
                .context("thread account rotation revision overflow")?;
            let revision = i64::try_from(policy.revision)?;
            let automatic_slot_ids_json =
                serde_json::to_string(&policy.automatic_account_slot_ids)?;
            affected.push((
                thread_id,
                previous_revision,
                revision,
                automatic_slot_ids_json,
                policy,
            ));
        }

        for (thread_id, previous_revision, revision, automatic_slot_ids_json, _) in &affected {
            let updated = sqlx::query(
                "UPDATE thread_account_rotation_policies SET revision = ?, \
                 automatic_slot_ids_json = ?, updated_at = unixepoch() \
                 WHERE thread_id = ? AND revision = ?",
            )
            .bind(revision)
            .bind(automatic_slot_ids_json)
            .bind(thread_id.to_string())
            .bind(previous_revision)
            .execute(&mut *transaction)
            .await?;
            if updated.rows_affected() != 1 {
                transaction.rollback().await?;
                anyhow::bail!("thread account rotation changed during credential invalidation");
            }
        }
        transaction.commit().await?;
        Ok(affected
            .into_iter()
            .map(|(thread_id, _, _, _, policy)| (thread_id, policy))
            .collect())
    }

    pub async fn compare_and_swap_execution_account_binding_with_intent(
        &self,
        thread_id: ThreadId,
        expected: &ExecutionAccountBinding,
        next_slot_id: &str,
        intent: AccountBindingCommitIntent,
    ) -> anyhow::Result<Option<ExecutionAccountBinding>> {
        if next_slot_id.is_empty() {
            anyhow::bail!("next execution account slot must not be empty");
        }
        let next_generation = expected
            .generation
            .checked_add(1)
            .context("execution account generation overflow")?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let binding_updated = sqlx::query(
            "UPDATE thread_execution_account_bindings SET slot_id = ?, generation = ? \
             WHERE thread_id = ? AND slot_id = ? AND generation = ?",
        )
        .bind(next_slot_id)
        .bind(i64::try_from(next_generation)?)
        .bind(thread_id.to_string())
        .bind(&expected.slot_id)
        .bind(i64::try_from(expected.generation)?)
        .execute(&mut *transaction)
        .await?;
        if binding_updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(None);
        }

        if intent == AccountBindingCommitIntent::PinFixed {
            pin_fixed_policy(&mut transaction, thread_id, next_slot_id).await?;
        }
        transaction.commit().await?;
        Ok(Some(ExecutionAccountBinding {
            slot_id: next_slot_id.to_string(),
            generation: next_generation,
        }))
    }
}

fn default_execution_account_binding() -> ExecutionAccountBinding {
    ExecutionAccountBinding {
        slot_id: DEFAULT_ACCOUNT_SLOT_ID.to_string(),
        generation: 1,
    }
}

async fn pin_fixed_policy(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: ThreadId,
    next_slot_id: &str,
) -> anyhow::Result<()> {
    let current = read_policy(&mut **transaction, thread_id).await?;
    match current {
        Some(current) => {
            let next_revision = current
                .revision
                .checked_add(1)
                .context("thread account rotation revision overflow")?;
            let updated = sqlx::query(
                "UPDATE thread_account_rotation_policies SET revision = ?, mode = 'fixed', \
                 fixed_slot_id = ?, last_committed_slot_id = ?, updated_at = unixepoch() \
                 WHERE thread_id = ? AND revision = ?",
            )
            .bind(i64::try_from(next_revision)?)
            .bind(next_slot_id)
            .bind(next_slot_id)
            .bind(thread_id.to_string())
            .bind(i64::try_from(current.revision)?)
            .execute(&mut **transaction)
            .await?;
            if updated.rows_affected() != 1 {
                anyhow::bail!("thread account rotation changed during binding commit");
            }
        }
        None => {
            sqlx::query(
                "INSERT INTO thread_account_rotation_policies \
                 (thread_id, revision, mode, fixed_slot_id, automatic_slot_ids_json, \
                  last_committed_slot_id, updated_at) \
                 VALUES (?, 1, 'fixed', ?, '[]', ?, unixepoch())",
            )
            .bind(thread_id.to_string())
            .bind(next_slot_id)
            .bind(next_slot_id)
            .execute(&mut **transaction)
            .await?;
        }
    }
    Ok(())
}

async fn read_policy<'e, E>(
    executor: E,
    thread_id: ThreadId,
) -> anyhow::Result<Option<ThreadAccountRotationPolicy>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query(
        "SELECT revision, mode, fixed_slot_id, automatic_slot_ids_json, \
         last_committed_slot_id FROM thread_account_rotation_policies WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_optional(executor)
    .await?;
    row.map(|row| policy_from_row(&row)).transpose()
}

fn policy_from_row(row: &SqliteRow) -> anyhow::Result<ThreadAccountRotationPolicy> {
    let automatic_slot_ids_json = row.try_get::<String, _>("automatic_slot_ids_json")?;
    let policy = ThreadAccountRotationPolicy {
        mode: ThreadAccountRotationMode::from_db_str(row.try_get("mode")?)?,
        fixed_account_slot_id: row.try_get("fixed_slot_id")?,
        automatic_account_slot_ids: serde_json::from_str(&automatic_slot_ids_json)
            .context("invalid automatic account slot ids")?,
        revision: u64::try_from(row.try_get::<i64, _>("revision")?)?,
        last_committed_account_slot_id: row.try_get("last_committed_slot_id")?,
    };
    ThreadAccountRotationPolicyUpdate {
        mode: policy.mode,
        fixed_account_slot_id: policy.fixed_account_slot_id.clone(),
        automatic_account_slot_ids: policy.automatic_account_slot_ids.clone(),
    }
    .validate_persisted()
    .context("invalid stored thread account rotation policy")?;
    Ok(policy)
}

#[cfg(test)]
#[path = "account_rotation_tests.rs"]
mod tests;
