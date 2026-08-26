use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_app_server_protocol::RateLimitSnapshot;
use tokio::sync::Mutex;
use tokio::sync::OnceCell;

const QUOTA_TTL: Duration = Duration::from_secs(60);
const QUOTA_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct QuotaCacheKey {
    pub(crate) account_slot_id: String,
    pub(crate) attempt_generation: u64,
    pub(crate) runtime_version: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct QuotaSnapshot {
    pub(crate) captured_at: i64,
    pub(crate) rate_limits: RateLimitSnapshot,
    pub(crate) rate_limits_by_limit_id: HashMap<String, RateLimitSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum QuotaFetchError {
    Unsupported,
    Transient(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum QuotaCacheLookup {
    Fresh(Box<QuotaSnapshot>),
    Unsupported,
    MissingOrStale,
}

#[derive(Clone, Debug)]
enum CachedQuota {
    Snapshot {
        value: Box<QuotaSnapshot>,
        expires_at: Instant,
    },
    Unsupported,
}

#[derive(Default)]
pub(crate) struct QuotaCache {
    entries: Mutex<HashMap<QuotaCacheKey, Arc<OnceCell<CachedQuota>>>>,
}

impl QuotaCache {
    pub(crate) async fn lookup(&self, key: &QuotaCacheKey) -> QuotaCacheLookup {
        let entry = self.entries.lock().await.get(key).cloned();
        match entry.as_deref().and_then(OnceCell::get) {
            Some(CachedQuota::Snapshot { value, expires_at }) if *expires_at > Instant::now() => {
                QuotaCacheLookup::Fresh(value.clone())
            }
            Some(CachedQuota::Unsupported) => QuotaCacheLookup::Unsupported,
            Some(CachedQuota::Snapshot { .. }) | None => QuotaCacheLookup::MissingOrStale,
        }
    }

    pub(crate) async fn read_or_fetch<F, Fut>(
        &self,
        key: QuotaCacheKey,
        fetch: F,
    ) -> Result<QuotaSnapshot, QuotaFetchError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<QuotaSnapshot, QuotaFetchError>>,
    {
        loop {
            let entry = {
                let mut entries = self.entries.lock().await;
                entries.retain(|existing, _| {
                    existing.account_slot_id != key.account_slot_id || existing == &key
                });
                if entries.get(&key).is_some_and(|entry| {
                    matches!(entry.get(), Some(CachedQuota::Snapshot { expires_at, .. }) if *expires_at <= Instant::now())
                }) {
                    entries.remove(&key);
                }
                Arc::clone(
                    entries
                        .entry(key.clone())
                        .or_insert_with(|| Arc::new(OnceCell::new())),
                )
            };
            let cached = entry
                .get_or_try_init(|| async {
                    match tokio::time::timeout(QUOTA_FETCH_TIMEOUT, fetch()).await {
                        Ok(Ok(value)) => Ok(CachedQuota::Snapshot {
                            value: Box::new(value),
                            expires_at: Instant::now() + QUOTA_TTL,
                        }),
                        Ok(Err(QuotaFetchError::Unsupported)) => Ok(CachedQuota::Unsupported),
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(QuotaFetchError::Transient(
                            "account slot rate-limit fetch timed out".to_string(),
                        )),
                    }
                })
                .await?;
            let still_current = self
                .entries
                .lock()
                .await
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &entry));
            if !still_current {
                continue;
            }
            return match cached {
                CachedQuota::Snapshot { value, .. } => Ok(value.as_ref().clone()),
                CachedQuota::Unsupported => Err(QuotaFetchError::Unsupported),
            };
        }
    }

    pub(crate) async fn invalidate_slot(&self, account_slot_id: &str) {
        self.entries.lock().await.retain(|key, entry| {
            key.account_slot_id != account_slot_id
                || matches!(entry.get(), Some(CachedQuota::Unsupported))
        });
    }
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
