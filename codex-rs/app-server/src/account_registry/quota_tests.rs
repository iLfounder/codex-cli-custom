use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use super::*;

fn key() -> QuotaCacheKey {
    QuotaCacheKey {
        account_slot_id: "slot".to_string(),
        attempt_generation: 2,
        runtime_version: 3,
    }
}

#[tokio::test]
async fn lookup_never_fetches_and_treats_expired_snapshots_as_missing() {
    let cache = QuotaCache::default();
    assert_eq!(cache.lookup(&key()).await, QuotaCacheLookup::MissingOrStale);

    let value = QuotaSnapshot {
        captured_at: 10,
        rate_limits: empty_snapshot(),
        rate_limits_by_limit_id: HashMap::new(),
    };
    let fresh = Arc::new(OnceCell::new());
    fresh
        .set(CachedQuota::Snapshot {
            value: Box::new(value.clone()),
            expires_at: Instant::now() + Duration::from_secs(1),
        })
        .unwrap();
    cache.entries.lock().await.insert(key(), fresh);
    assert_eq!(
        cache.lookup(&key()).await,
        QuotaCacheLookup::Fresh(Box::new(value.clone()))
    );

    let expired = Arc::new(OnceCell::new());
    expired
        .set(CachedQuota::Snapshot {
            value: Box::new(value),
            expires_at: Instant::now(),
        })
        .unwrap();
    cache.entries.lock().await.insert(key(), expired);
    assert_eq!(cache.lookup(&key()).await, QuotaCacheLookup::MissingOrStale);
}

#[tokio::test]
async fn concurrent_reads_share_one_fetch_and_targeted_invalidation_refetches() {
    let cache = Arc::new(QuotaCache::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let read = |cache: Arc<QuotaCache>, calls: Arc<AtomicUsize>| async move {
        cache
            .read_or_fetch(key(), || async {
                calls.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
                Ok(QuotaSnapshot {
                    captured_at: 10,
                    rate_limits: empty_snapshot(),
                    rate_limits_by_limit_id: HashMap::new(),
                })
            })
            .await
    };
    let (left, right) = tokio::join!(
        read(Arc::clone(&cache), Arc::clone(&calls)),
        read(Arc::clone(&cache), Arc::clone(&calls))
    );
    assert_eq!(left, right);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    cache.invalidate_slot("slot").await;
    read(Arc::clone(&cache), Arc::clone(&calls)).await.unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn unsupported_is_terminal_for_the_exact_key() {
    let cache = QuotaCache::default();
    let calls = AtomicUsize::new(0);
    for _ in 0..2 {
        assert_eq!(
            cache
                .read_or_fetch(key(), || async {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Err(QuotaFetchError::Unsupported)
                })
                .await,
            Err(QuotaFetchError::Unsupported)
        );
    }
    cache.invalidate_slot("slot").await;
    assert_eq!(
        cache
            .read_or_fetch(key(), || async {
                calls.fetch_add(1, Ordering::Relaxed);
                Err(QuotaFetchError::Unsupported)
            })
            .await,
        Err(QuotaFetchError::Unsupported)
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(cache.lookup(&key()).await, QuotaCacheLookup::Unsupported);
    let mut next_runtime = key();
    next_runtime.runtime_version += 1;
    assert_eq!(
        cache
            .read_or_fetch(next_runtime, || async {
                calls.fetch_add(1, Ordering::Relaxed);
                Err(QuotaFetchError::Unsupported)
            })
            .await,
        Err(QuotaFetchError::Unsupported)
    );
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

fn empty_snapshot() -> RateLimitSnapshot {
    RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        primary: None,
        secondary: None,
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }
}
