use anyhow::Context;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use serde::Deserialize;
use serde::Serialize;

use super::StateRuntime;
use super::account_rotation_cursor::ThreadAccountRotationCursor;
use super::account_rotation_cursor::read_cursor;
use super::account_rotation_cursor::write_cursor;
use super::account_rotation_profile::AccountRotationProfile;
use super::account_rotation_profile::AccountRotationProfileUpdate;
use super::account_rotation_profile::ThreadAccountRotationMode;
use super::account_rotation_profile::read_global_profile;
use super::account_rotation_profile::read_thread_override;
use super::account_rotation_profile::write_thread_override;

const DEFAULT_ACCOUNT_SLOT_ID: &str = "default";

pub type ThreadAccountRotationPolicyUpdate = AccountRotationProfileUpdate;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountBindingCommitIntent {
    PreserveRotation,
    PinFixed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuccessfulAccountBindingTransition {
    Keep,
    AdvanceGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SuccessfulAccountRotationCommit {
    pub binding: ExecutionAccountBinding,
    pub policy: ThreadAccountRotationPolicy,
}

impl StateRuntime {
    pub async fn thread_account_rotation_policy(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<ThreadAccountRotationPolicy> {
        let binding = self
            .execution_account_binding(thread_id)
            .await?
            .unwrap_or_else(default_execution_account_binding);
        let profile = read_thread_override(self.pool.as_ref(), thread_id)
            .await?
            .or(read_global_profile(self.pool.as_ref()).await?);
        let cursor = read_cursor(self.pool.as_ref(), thread_id).await?;
        Ok(effective_policy(profile, &binding, cursor))
    }

    /// Compatibility entrypoint: a thread-scoped update always creates or updates an override.
    pub async fn compare_and_swap_thread_account_rotation_policy(
        &self,
        thread_id: ThreadId,
        expected_revision: u64,
        update: &ThreadAccountRotationPolicyUpdate,
    ) -> anyhow::Result<Option<ThreadAccountRotationPolicy>> {
        let Some(profile) = self
            .compare_and_swap_thread_account_rotation_override(thread_id, expected_revision, update)
            .await?
        else {
            return Ok(None);
        };
        let binding = self
            .execution_account_binding(thread_id)
            .await?
            .unwrap_or_else(default_execution_account_binding);
        let cursor = read_cursor(self.pool.as_ref(), thread_id).await?;
        Ok(Some(effective_policy(Some(profile), &binding, cursor)))
    }

    pub async fn compare_and_swap_thread_account_rotation_cursor_for_binding(
        &self,
        thread_id: ThreadId,
        expected_binding: &ExecutionAccountBinding,
        accepted_account_slot_id: &str,
    ) -> anyhow::Result<Option<ThreadAccountRotationPolicy>> {
        if accepted_account_slot_id.is_empty() {
            anyhow::bail!("accepted account slot must not be empty");
        }
        if accepted_account_slot_id != expected_binding.slot_id {
            anyhow::bail!("cursor account slot must match the expected execution account binding");
        }
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if !binding_matches(&mut transaction, thread_id, expected_binding).await? {
            transaction.rollback().await?;
            return Ok(None);
        }
        let cursor = write_cursor(&mut transaction, thread_id, accepted_account_slot_id).await?;
        let policy = effective_policy_in_transaction(
            &mut transaction,
            thread_id,
            expected_binding,
            Some(cursor),
        )
        .await?;
        transaction.commit().await?;
        Ok(Some(policy))
    }

    pub async fn compare_and_swap_successful_account_rotation(
        &self,
        thread_id: ThreadId,
        expected_binding: &ExecutionAccountBinding,
        accepted_account_slot_id: &str,
        binding_transition: SuccessfulAccountBindingTransition,
    ) -> anyhow::Result<Option<SuccessfulAccountRotationCommit>> {
        if accepted_account_slot_id.is_empty() {
            anyhow::bail!("accepted account slot must not be empty");
        }
        if binding_transition == SuccessfulAccountBindingTransition::Keep
            && accepted_account_slot_id != expected_binding.slot_id
        {
            anyhow::bail!("kept execution account binding must match the accepted account slot");
        }
        let next_generation = match binding_transition {
            SuccessfulAccountBindingTransition::Keep => expected_binding.generation,
            SuccessfulAccountBindingTransition::AdvanceGeneration => expected_binding
                .generation
                .checked_add(1)
                .context("execution account generation overflow")?,
        };
        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if !binding_matches(&mut transaction, thread_id, expected_binding).await? {
            transaction.rollback().await?;
            return Ok(None);
        }
        if binding_transition == SuccessfulAccountBindingTransition::AdvanceGeneration {
            let updated = sqlx::query(
                "UPDATE thread_execution_account_bindings SET slot_id = ?, generation = ? \
                 WHERE thread_id = ? AND slot_id = ? AND generation = ?",
            )
            .bind(accepted_account_slot_id)
            .bind(i64::try_from(next_generation)?)
            .bind(thread_id.to_string())
            .bind(&expected_binding.slot_id)
            .bind(i64::try_from(expected_binding.generation)?)
            .execute(&mut *transaction)
            .await?;
            if updated.rows_affected() != 1 {
                transaction.rollback().await?;
                return Ok(None);
            }
        }
        let binding = ExecutionAccountBinding {
            slot_id: accepted_account_slot_id.to_string(),
            generation: next_generation,
        };
        let cursor = write_cursor(&mut transaction, thread_id, accepted_account_slot_id).await?;
        let policy =
            effective_policy_in_transaction(&mut transaction, thread_id, &binding, Some(cursor))
                .await?;
        transaction.commit().await?;
        Ok(Some(SuccessfulAccountRotationCommit { binding, policy }))
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
            pin_fixed_override(&mut transaction, thread_id, next_slot_id).await?;
            write_cursor(&mut transaction, thread_id, next_slot_id).await?;
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

async fn pin_fixed_override(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: ThreadId,
    next_slot_id: &str,
) -> anyhow::Result<()> {
    let current = read_thread_override(&mut **transaction, thread_id).await?;
    let update = AccountRotationProfileUpdate {
        mode: ThreadAccountRotationMode::Fixed,
        fixed_account_slot_id: Some(next_slot_id.to_string()),
        automatic_account_slot_ids: current.as_ref().map_or_else(Vec::new, |profile| {
            profile.automatic_account_slot_ids.clone()
        }),
    };
    write_thread_override(
        transaction,
        thread_id,
        current.as_ref().map_or(0, |profile| profile.revision),
        &update,
    )
    .await?;
    Ok(())
}

async fn binding_matches(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: ThreadId,
    expected: &ExecutionAccountBinding,
) -> anyhow::Result<bool> {
    let current = sqlx::query_as::<_, (String, i64)>(
        "SELECT slot_id, generation FROM thread_execution_account_bindings WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(current.is_some_and(|(slot_id, generation)| {
        slot_id == expected.slot_id && u64::try_from(generation).ok() == Some(expected.generation)
    }))
}

async fn effective_policy_in_transaction(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: ThreadId,
    binding: &ExecutionAccountBinding,
    cursor: Option<ThreadAccountRotationCursor>,
) -> anyhow::Result<ThreadAccountRotationPolicy> {
    let profile = read_thread_override(&mut **transaction, thread_id)
        .await?
        .or(read_global_profile(&mut **transaction).await?);
    Ok(effective_policy(profile, binding, cursor))
}

fn effective_policy(
    profile: Option<AccountRotationProfile>,
    binding: &ExecutionAccountBinding,
    cursor: Option<ThreadAccountRotationCursor>,
) -> ThreadAccountRotationPolicy {
    let Some(profile) = profile else {
        return ThreadAccountRotationPolicy::virtual_fixed(binding);
    };
    ThreadAccountRotationPolicy {
        mode: profile.mode,
        fixed_account_slot_id: profile.fixed_account_slot_id,
        automatic_account_slot_ids: profile.automatic_account_slot_ids,
        revision: profile.revision,
        last_committed_account_slot_id: cursor.map(|cursor| cursor.last_committed_account_slot_id),
    }
}

#[cfg(test)]
#[path = "account_rotation_tests.rs"]
mod tests;
