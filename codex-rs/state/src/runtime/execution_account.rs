use anyhow::Context;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use sqlx::Row;

use super::StateRuntime;

impl StateRuntime {
    /// Returns one transactionally consistent runtime version and complete durable binding set.
    ///
    /// Slots created before runtime versioning have no row and therefore read as version zero.
    pub async fn execution_account_slot_runtime_state(
        &self,
        slot_id: &str,
    ) -> anyhow::Result<(u64, Vec<(ThreadId, ExecutionAccountBinding)>)> {
        let mut transaction = self.pool.begin().await?;
        let runtime_version = slot_runtime_version(&mut transaction, slot_id).await?;
        let bindings = slot_bindings(&mut transaction, slot_id).await?;
        transaction.commit().await?;
        Ok((runtime_version, bindings))
    }

    /// Advances one slot runtime and every exact expected binding in one transaction.
    ///
    /// Returns `None` without mutation when either the version or the complete binding set changed.
    /// A successful commit increments the runtime version and every binding generation exactly once.
    pub async fn compare_and_swap_execution_account_slot_runtime(
        &self,
        slot_id: &str,
        expected_runtime_version: u64,
        expected_bindings: &[(ThreadId, ExecutionAccountBinding)],
    ) -> anyhow::Result<Option<(u64, Vec<(ThreadId, ExecutionAccountBinding)>)>> {
        let next_runtime_version = expected_runtime_version
            .checked_add(1)
            .context("execution account runtime version overflow")?;
        let next_runtime_version_i64 = i64::try_from(next_runtime_version)?;
        let expected_runtime_version_i64 = i64::try_from(expected_runtime_version)?;
        let mut expected_bindings = expected_bindings.to_vec();
        expected_bindings.sort_by_key(|(thread_id, _)| thread_id.to_string());
        let mut next_bindings = Vec::with_capacity(expected_bindings.len());
        for (thread_id, binding) in &expected_bindings {
            if binding.slot_id != slot_id {
                anyhow::bail!(
                    "thread {thread_id} belongs to slot {}, not {slot_id}",
                    binding.slot_id
                );
            }
            let generation = binding
                .generation
                .checked_add(1)
                .context("execution account generation overflow")?;
            let _ = i64::try_from(generation)?;
            next_bindings.push((
                *thread_id,
                ExecutionAccountBinding {
                    slot_id: slot_id.to_string(),
                    generation,
                },
            ));
        }
        if expected_bindings
            .windows(2)
            .any(|bindings| bindings[0].0 == bindings[1].0)
        {
            anyhow::bail!("expected execution account binding set contains duplicate threads");
        }

        let mut transaction = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current_runtime_version = slot_runtime_version(&mut transaction, slot_id).await?;
        let current_bindings = slot_bindings(&mut transaction, slot_id).await?;
        if current_runtime_version != expected_runtime_version
            || current_bindings != expected_bindings
        {
            transaction.rollback().await?;
            return Ok(None);
        }

        let runtime_version_updated = sqlx::query(
            "INSERT INTO account_slot_runtime_versions (slot_id, runtime_version) VALUES (?, ?) \
             ON CONFLICT(slot_id) DO UPDATE SET runtime_version = excluded.runtime_version \
             WHERE account_slot_runtime_versions.runtime_version = ?",
        )
        .bind(slot_id)
        .bind(next_runtime_version_i64)
        .bind(expected_runtime_version_i64)
        .execute(&mut *transaction)
        .await?;
        if runtime_version_updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Ok(None);
        }
        for ((thread_id, expected), (_, next)) in expected_bindings.iter().zip(&next_bindings) {
            let binding_updated = sqlx::query(
                "UPDATE thread_execution_account_bindings SET generation = ? \
                 WHERE thread_id = ? AND slot_id = ? AND generation = ?",
            )
            .bind(i64::try_from(next.generation)?)
            .bind(thread_id.to_string())
            .bind(slot_id)
            .bind(i64::try_from(expected.generation)?)
            .execute(&mut *transaction)
            .await?;
            if binding_updated.rows_affected() != 1 {
                transaction.rollback().await?;
                return Ok(None);
            }
        }
        transaction.commit().await?;
        Ok(Some((next_runtime_version, next_bindings)))
    }

    pub async fn execution_account_binding(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<ExecutionAccountBinding>> {
        let row = sqlx::query(
            "SELECT slot_id, generation FROM thread_execution_account_bindings WHERE thread_id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(binding_from_row).transpose()
    }

    pub async fn initialize_execution_account_binding(
        &self,
        thread_id: ThreadId,
        initial: &ExecutionAccountBinding,
    ) -> anyhow::Result<ExecutionAccountBinding> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO thread_execution_account_bindings (thread_id, slot_id, generation) \
             VALUES (?, ?, ?) ON CONFLICT(thread_id) DO NOTHING",
        )
        .bind(thread_id.to_string())
        .bind(&initial.slot_id)
        .bind(i64::try_from(initial.generation)?)
        .execute(&mut *transaction)
        .await?;
        let row = sqlx::query(
            "SELECT slot_id, generation FROM thread_execution_account_bindings WHERE thread_id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        binding_from_row(row)
    }

    pub async fn compare_and_swap_execution_account_binding(
        &self,
        thread_id: ThreadId,
        expected: &ExecutionAccountBinding,
        next_slot_id: &str,
    ) -> anyhow::Result<Option<ExecutionAccountBinding>> {
        let generation = i64::try_from(expected.generation)?
            .checked_add(1)
            .context("execution account generation overflow")?;
        let result = sqlx::query(
            "UPDATE thread_execution_account_bindings SET slot_id = ?, generation = ? \
             WHERE thread_id = ? AND slot_id = ? AND generation = ?",
        )
        .bind(next_slot_id)
        .bind(generation)
        .bind(thread_id.to_string())
        .bind(&expected.slot_id)
        .bind(i64::try_from(expected.generation)?)
        .execute(self.pool.as_ref())
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        Ok(Some(ExecutionAccountBinding {
            slot_id: next_slot_id.to_string(),
            generation: u64::try_from(generation)?,
        }))
    }

    pub async fn record_turn_execution_account(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
        binding: &ExecutionAccountBinding,
    ) -> anyhow::Result<()> {
        let result = sqlx::query(
            "INSERT INTO thread_turn_execution_accounts (thread_id, turn_id, slot_id, generation) \
             VALUES (?, ?, ?, ?) ON CONFLICT(thread_id, turn_id) DO NOTHING",
        )
        .bind(thread_id.to_string())
        .bind(turn_id)
        .bind(&binding.slot_id)
        .bind(i64::try_from(binding.generation)?)
        .execute(self.pool.as_ref())
        .await?;
        if result.rows_affected() == 1 {
            return Ok(());
        }
        let existing = self
            .turn_execution_account(thread_id, turn_id)
            .await?
            .context("turn execution account disappeared after conflict")?;
        if existing == *binding {
            Ok(())
        } else {
            anyhow::bail!(
                "turn {turn_id} for thread {thread_id} already has a different execution account"
            )
        }
    }

    pub async fn turn_execution_account(
        &self,
        thread_id: ThreadId,
        turn_id: &str,
    ) -> anyhow::Result<Option<ExecutionAccountBinding>> {
        let row = sqlx::query(
            "SELECT slot_id, generation FROM thread_turn_execution_accounts \
             WHERE thread_id = ? AND turn_id = ?",
        )
        .bind(thread_id.to_string())
        .bind(turn_id)
        .fetch_optional(self.pool.as_ref())
        .await?;
        row.map(binding_from_row).transpose()
    }
}

async fn slot_runtime_version(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    slot_id: &str,
) -> anyhow::Result<u64> {
    let runtime_version = sqlx::query_scalar::<_, i64>(
        "SELECT runtime_version FROM account_slot_runtime_versions WHERE slot_id = ?",
    )
    .bind(slot_id)
    .fetch_optional(&mut **transaction)
    .await?;
    Ok(runtime_version
        .map(u64::try_from)
        .transpose()?
        .unwrap_or_default())
}

async fn slot_bindings(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    slot_id: &str,
) -> anyhow::Result<Vec<(ThreadId, ExecutionAccountBinding)>> {
    sqlx::query(
        "SELECT thread_id, slot_id, generation FROM thread_execution_account_bindings \
         WHERE slot_id = ? ORDER BY thread_id",
    )
    .bind(slot_id)
    .fetch_all(&mut **transaction)
    .await?
    .into_iter()
    .map(|row| -> anyhow::Result<_> {
        let thread_id = row.try_get::<String, _>("thread_id")?;
        Ok((
            ThreadId::from_string(&thread_id)
                .with_context(|| format!("invalid execution account thread id {thread_id}"))?,
            binding_from_row(row)?,
        ))
    })
    .collect()
}

fn binding_from_row(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<ExecutionAccountBinding> {
    Ok(ExecutionAccountBinding {
        slot_id: row.try_get("slot_id")?,
        generation: u64::try_from(row.try_get::<i64, _>("generation")?)?,
    })
}

#[cfg(test)]
#[path = "execution_account_tests.rs"]
mod tests;
