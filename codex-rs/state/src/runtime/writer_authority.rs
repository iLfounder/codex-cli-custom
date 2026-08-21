use codex_protocol::ThreadId;
use sqlx::SqlitePool;
use uuid::Uuid;

use super::StateRuntime;

/// Durable identity and generation allocated to one thread writer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriterGeneration {
    pub store_id: String,
    pub generation: u64,
}

pub(super) async fn load_or_create_writer_store_id(pool: &SqlitePool) -> anyhow::Result<String> {
    let candidate = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO writer_authority_store (singleton, store_id) VALUES (1, ?) \
         ON CONFLICT(singleton) DO NOTHING",
    )
    .bind(candidate)
    .execute(pool)
    .await?;

    sqlx::query_scalar("SELECT store_id FROM writer_authority_store WHERE singleton = 1")
        .fetch_one(pool)
        .await
        .map_err(anyhow::Error::from)
}

impl StateRuntime {
    /// Return the opaque identity persisted by this state store.
    pub fn writer_store_id(&self) -> &str {
        &self.writer_store_id
    }

    /// Allocate the next durable writer generation for `thread_id`.
    ///
    /// The existing state pool owns this transaction so callers never reopen the
    /// database or extend a filesystem coordination-lock critical section.
    pub async fn next_writer_generation(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<WriterGeneration> {
        let mut transaction = self.pool.begin().await?;
        let generation = sqlx::query_scalar::<_, i64>(
            r#"
INSERT INTO thread_writer_generations (thread_id, generation)
VALUES (?, 1)
ON CONFLICT(thread_id) DO UPDATE SET generation = generation + 1
RETURNING generation
            "#,
        )
        .bind(thread_id.to_string())
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(WriterGeneration {
            store_id: self.writer_store_id.clone(),
            generation: u64::try_from(generation)?,
        })
    }

    /// Read the latest durable generation without acquiring writer ownership.
    pub async fn writer_generation(
        &self,
        thread_id: ThreadId,
    ) -> anyhow::Result<Option<WriterGeneration>> {
        let generation = sqlx::query_scalar::<_, i64>(
            "SELECT generation FROM thread_writer_generations WHERE thread_id = ?",
        )
        .bind(thread_id.to_string())
        .fetch_optional(self.pool.as_ref())
        .await?;
        generation
            .map(|generation| {
                Ok(WriterGeneration {
                    store_id: self.writer_store_id.clone(),
                    generation: u64::try_from(generation)?,
                })
            })
            .transpose()
    }
}

#[cfg(test)]
#[path = "writer_authority_tests.rs"]
mod tests;
