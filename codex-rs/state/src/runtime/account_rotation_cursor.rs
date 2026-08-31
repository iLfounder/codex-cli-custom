use codex_protocol::ThreadId;
use sqlx::Row;

use super::StateRuntime;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ThreadAccountRotationCursor {
    pub last_committed_account_slot_id: String,
}

impl StateRuntime {
    pub async fn thread_account_rotation_cursor(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<ThreadAccountRotationCursor>> {
        read_cursor(self.pool.as_ref(), thread_id).await
    }
}

pub(super) async fn read_cursor<'e, E>(
    executor: E,
    thread_id: ThreadId,
) -> anyhow::Result<Option<ThreadAccountRotationCursor>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query(
        "SELECT last_committed_slot_id FROM thread_account_rotation_cursors \
         WHERE thread_id = ?",
    )
    .bind(thread_id.to_string())
    .fetch_optional(executor)
    .await?;
    row.map(|row| {
        Ok(ThreadAccountRotationCursor {
            last_committed_account_slot_id: row.try_get("last_committed_slot_id")?,
        })
    })
    .transpose()
}

pub(super) async fn write_cursor(
    transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    thread_id: ThreadId,
    accepted_account_slot_id: &str,
) -> anyhow::Result<ThreadAccountRotationCursor> {
    sqlx::query(
        "INSERT INTO thread_account_rotation_cursors \
         (thread_id, last_committed_slot_id, updated_at) VALUES (?, ?, unixepoch()) \
         ON CONFLICT(thread_id) DO UPDATE SET \
         last_committed_slot_id = excluded.last_committed_slot_id, \
         updated_at = excluded.updated_at",
    )
    .bind(thread_id.to_string())
    .bind(accepted_account_slot_id)
    .execute(&mut **transaction)
    .await?;
    Ok(ThreadAccountRotationCursor {
        last_committed_account_slot_id: accepted_account_slot_id.to_string(),
    })
}
