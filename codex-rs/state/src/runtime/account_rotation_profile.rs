use std::collections::HashSet;

use anyhow::Context;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::StateRuntime;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadAccountRotationMode {
    Fixed,
    QuotaAware,
    RoundRobin,
    ExhaustThenNext,
}

impl ThreadAccountRotationMode {
    pub(super) fn as_db_str(self) -> &'static str {
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
            _ => anyhow::bail!("invalid account rotation mode {value}"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRotationProfile {
    pub mode: ThreadAccountRotationMode,
    pub fixed_account_slot_id: Option<String>,
    pub automatic_account_slot_ids: Vec<String>,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountRotationProfileUpdate {
    pub mode: ThreadAccountRotationMode,
    pub fixed_account_slot_id: Option<String>,
    pub automatic_account_slot_ids: Vec<String>,
}

impl AccountRotationProfileUpdate {
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

impl StateRuntime {
    /// Returns `None` before the singleton global profile is activated. Its CAS revision is zero.
    pub async fn account_rotation_global_profile(
        &self,
    ) -> anyhow::Result<Option<AccountRotationProfile>> {
        read_global_profile(self.pool.as_ref()).await
    }

    pub async fn compare_and_swap_account_rotation_global_profile(
        &self,
        expected_revision: u64,
        update: &AccountRotationProfileUpdate,
    ) -> anyhow::Result<Option<AccountRotationProfile>> {
        update.validate()?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = read_global_profile(&mut *transaction).await?;
        if current.as_ref().map_or(0, |profile| profile.revision) != expected_revision {
            transaction.rollback().await?;
            return Ok(None);
        }
        let next_revision = expected_revision
            .checked_add(1)
            .context("global account rotation revision overflow")?;
        let automatic_slot_ids_json = serde_json::to_string(&update.automatic_account_slot_ids)?;
        let result = sqlx::query(
            "INSERT INTO account_rotation_global_profile \
             (singleton, revision, mode, fixed_slot_id, automatic_slot_ids_json, updated_at) \
             VALUES (1, ?, ?, ?, ?, unixepoch()) ON CONFLICT(singleton) DO UPDATE SET \
             revision = excluded.revision, mode = excluded.mode, \
             fixed_slot_id = excluded.fixed_slot_id, \
             automatic_slot_ids_json = excluded.automatic_slot_ids_json, \
             updated_at = excluded.updated_at WHERE revision = ?",
        )
        .bind(i64::try_from(next_revision)?)
        .bind(update.mode.as_db_str())
        .bind(&update.fixed_account_slot_id)
        .bind(automatic_slot_ids_json)
        .bind(i64::try_from(expected_revision)?)
        .execute(&mut *transaction)
        .await?;
        if result.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(None);
        }
        let committed = read_global_profile(&mut *transaction)
            .await?
            .context("global account rotation profile disappeared after commit")?;
        transaction.commit().await?;
        Ok(Some(committed))
    }

    pub async fn thread_account_rotation_override(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<AccountRotationProfile>> {
        read_thread_override(self.pool.as_ref(), thread_id).await
    }

    pub async fn compare_and_swap_thread_account_rotation_override(
        &self,
        thread_id: ThreadId,
        expected_revision: u64,
        update: &AccountRotationProfileUpdate,
    ) -> anyhow::Result<Option<AccountRotationProfile>> {
        update.validate()?;
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = read_thread_override(&mut *transaction, thread_id).await?;
        if current.as_ref().map_or(0, |profile| profile.revision) != expected_revision {
            transaction.rollback().await?;
            return Ok(None);
        }
        let committed =
            write_thread_override(&mut transaction, thread_id, expected_revision, update).await?;
        transaction.commit().await?;
        Ok(Some(committed))
    }

    pub async fn reset_thread_account_rotation_override(
        &self,
        thread_id: ThreadId,
        expected_revision: u64,
    ) -> anyhow::Result<bool> {
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let deleted = sqlx::query(
            "DELETE FROM thread_account_rotation_overrides \
             WHERE thread_id = ? AND revision = ?",
        )
        .bind(thread_id.to_string())
        .bind(i64::try_from(expected_revision)?)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(deleted.rows_affected() == 1)
    }
}

pub(super) async fn write_thread_override(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: ThreadId,
    expected_revision: u64,
    update: &AccountRotationProfileUpdate,
) -> anyhow::Result<AccountRotationProfile> {
    let next_revision = expected_revision
        .checked_add(1)
        .context("thread account rotation override revision overflow")?;
    let automatic_slot_ids_json = serde_json::to_string(&update.automatic_account_slot_ids)?;
    let result = sqlx::query(
        "INSERT INTO thread_account_rotation_overrides \
         (thread_id, revision, mode, fixed_slot_id, automatic_slot_ids_json, updated_at) \
         VALUES (?, ?, ?, ?, ?, unixepoch()) ON CONFLICT(thread_id) DO UPDATE SET \
         revision = excluded.revision, mode = excluded.mode, \
         fixed_slot_id = excluded.fixed_slot_id, \
         automatic_slot_ids_json = excluded.automatic_slot_ids_json, \
         updated_at = excluded.updated_at WHERE revision = ?",
    )
    .bind(thread_id.to_string())
    .bind(i64::try_from(next_revision)?)
    .bind(update.mode.as_db_str())
    .bind(&update.fixed_account_slot_id)
    .bind(automatic_slot_ids_json)
    .bind(i64::try_from(expected_revision)?)
    .execute(&mut **transaction)
    .await?;
    if result.rows_affected() != 1 {
        anyhow::bail!("thread account rotation override changed during commit");
    }
    read_thread_override(&mut **transaction, thread_id)
        .await?
        .context("thread account rotation override disappeared after commit")
}

pub(super) async fn read_global_profile<'e, E>(
    executor: E,
) -> anyhow::Result<Option<AccountRotationProfile>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query(
        "SELECT revision, mode, fixed_slot_id, automatic_slot_ids_json \
         FROM account_rotation_global_profile WHERE singleton = 1",
    )
    .fetch_optional(executor)
    .await?;
    row.map(|row| profile_from_row(&row)).transpose()
}

pub(super) async fn read_thread_override<'e, E>(
    executor: E,
    thread_id: ThreadId,
) -> anyhow::Result<Option<AccountRotationProfile>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query(
        "SELECT revision, mode, fixed_slot_id, automatic_slot_ids_json \
         FROM thread_account_rotation_overrides WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_optional(executor)
    .await?;
    row.map(|row| profile_from_row(&row)).transpose()
}

fn profile_from_row(row: &SqliteRow) -> anyhow::Result<AccountRotationProfile> {
    let automatic_slot_ids_json = row.try_get::<String, _>("automatic_slot_ids_json")?;
    let profile = AccountRotationProfile {
        mode: ThreadAccountRotationMode::from_db_str(row.try_get("mode")?)?,
        fixed_account_slot_id: row.try_get("fixed_slot_id")?,
        automatic_account_slot_ids: serde_json::from_str(&automatic_slot_ids_json)
            .context("invalid automatic account slot ids")?,
        revision: u64::try_from(row.try_get::<i64, _>("revision")?)?,
    };
    AccountRotationProfileUpdate {
        mode: profile.mode,
        fixed_account_slot_id: profile.fixed_account_slot_id.clone(),
        automatic_account_slot_ids: profile.automatic_account_slot_ids.clone(),
    }
    .validate_persisted()
    .context("invalid stored account rotation profile")?;
    Ok(profile)
}

#[cfg(test)]
#[path = "account_rotation_profile_tests.rs"]
mod tests;
