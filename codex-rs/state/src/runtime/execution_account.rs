use anyhow::Context;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ExecutionAccountBinding;
use sqlx::Row;

use super::StateRuntime;

impl StateRuntime {
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

fn binding_from_row(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<ExecutionAccountBinding> {
    Ok(ExecutionAccountBinding {
        slot_id: row.try_get("slot_id")?,
        generation: u64::try_from(row.try_get::<i64, _>("generation")?)?,
    })
}

#[cfg(test)]
#[path = "execution_account_tests.rs"]
mod tests;
