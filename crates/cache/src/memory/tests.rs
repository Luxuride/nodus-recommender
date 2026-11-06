//! Tests for all three in-process backends, driven by injected clocks.

use std::time::{Duration, Instant};

use super::{InMemoryBudget, InMemoryCache, InMemoryLock};
use crate::cache::{Cache, CacheError};
use crate::lock::LockGuard;

#[tokio::test]
async fn budget_allows_the_burst_then_refuses() -> Result<(), CacheError> {
    let budget = InMemoryBudget::new(1, 5);
    let start = Instant::now();

    for _ in 0..5 {
        assert!(budget.acquire_at("example.social", "token", start)?);
    }
    assert!(!budget.acquire_at("example.social", "token", start)?);

    // Exactly one token has accrued after one second, and taking it resets
    // the clock for the next.
    let later = start + Duration::from_secs(1);
    assert!(budget.acquire_at("example.social", "token", later)?);
    assert!(!budget.acquire_at("example.social", "token", later)?);

    // Buckets are per host, not global.
    assert!(budget.acquire_at("other.social", "token", start)?);
    Ok(())
}

#[tokio::test]
async fn budget_refills_continuously() -> Result<(), CacheError> {
    let budget = InMemoryBudget::new(2, 2);
    let start = Instant::now();

    assert!(budget.acquire_at("example.social", "token", start)?);
    assert!(budget.acquire_at("example.social", "token", start)?);
    assert!(!budget.acquire_at("example.social", "token", start)?);

    // Two tokens per second means one token after 500 ms.
    let half = start + Duration::from_millis(500);
    assert!(budget.acquire_at("example.social", "token", half)?);
    assert!(!budget.acquire_at("example.social", "token", half)?);
    Ok(())
}

#[tokio::test]
async fn a_hold_refuses_every_bucket_on_the_host_until_it_expires() -> Result<(), CacheError> {
    let budget = InMemoryBudget::new(1, 5);
    let start = Instant::now();
    budget.hold_at("example.social", Duration::from_secs(30), start)?;

    assert!(!budget.acquire_at("example.social", "token-a", start)?);
    assert!(!budget.acquire_at("example.social", "token-b", start)?);
    // Another host is unaffected.
    assert!(budget.acquire_at("other.social", "token-a", start)?);

    // The hold ends when the instance said it would, and the refusal above
    // did not consume the bucket.
    let later = start + Duration::from_secs(30);
    assert!(budget.acquire_at("example.social", "token-a", later)?);
    assert_eq!(
        budget.hint_at("example.social", "token-a", start),
        Duration::from_secs(30)
    );
    Ok(())
}

#[tokio::test]
async fn lock_is_exclusive_until_released_and_expires_on_ttl() -> Result<(), CacheError> {
    let lock = InMemoryLock::new();
    let start = Instant::now();

    let guard = lock
        .acquire_at("recompute", Duration::from_secs(30), start)?
        .expect("first acquire succeeds");
    assert!(
        lock.acquire_at("recompute", Duration::from_secs(30), start)?
            .is_none()
    );

    // Releasing makes it available again immediately.
    lock.unlock(&guard)?;
    let second = lock
        .acquire_at("recompute", Duration::from_secs(30), start)?
        .expect("acquire after release succeeds");

    // A different owner cannot release the current hold.
    let foreign = LockGuard::new("recompute", "foreign-owner");
    lock.unlock(&foreign)?;
    assert!(
        lock.acquire_at("recompute", Duration::from_secs(30), start)?
            .is_none()
    );
    lock.unlock(&second)?;

    // A hold past its TTL is gone even if nobody released it.
    let short = lock
        .acquire_at("fit_model", Duration::from_secs(1), start)?
        .expect("first acquire succeeds");
    let after_ttl = start + Duration::from_millis(1500);
    assert!(
        lock.acquire_at("fit_model", Duration::from_secs(1), after_ttl)?
            .is_some()
    );
    // The stale guard must not release the new owner's hold.
    lock.unlock(&short)?;
    assert!(
        lock.acquire_at("fit_model", Duration::from_secs(1), after_ttl)?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn cache_round_trips_a_value_and_forgets_it_after_the_ttl() -> Result<(), CacheError> {
    let cache = InMemoryCache::new();
    let start = Instant::now();
    let ttl = Duration::from_secs(60);

    cache.set_at("verify:abc", b"user-1", ttl, start)?;
    assert_eq!(cache.get_at("verify:abc", start)?, Some(b"user-1".to_vec()));

    // Still present just before the deadline, gone at it.
    assert!(
        cache
            .get_at(
                "verify:abc",
                (start + ttl)
                    .checked_sub(Duration::from_secs(1))
                    .expect("one second before expiry is still after the start")
            )?
            .is_some()
    );
    assert!(cache.get_at("verify:abc", start + ttl)?.is_none());

    // Deleting removes a live entry; deleting an absent key is a no-op.
    cache.set_at("feed:1", b"snapshot", ttl, start)?;
    cache.remove("feed:1")?;
    assert!(cache.get_at("feed:1", start)?.is_none());
    cache.remove("feed:1")?;
    Ok(())
}

#[tokio::test]
async fn cache_json_helpers_use_the_byte_methods() -> Result<(), CacheError> {
    let cache = InMemoryCache::new();
    let ttl = Duration::from_secs(60);
    let value = serde_json::json!({ "cursor": "next", "items": [1, 2, 3] });

    cache.set_json("feed:1:next", &value, ttl).await?;
    let read: Option<serde_json::Value> = cache.get_json("feed:1:next").await?;
    assert_eq!(read, Some(value));

    // A missing key is a miss, not an error.
    let missing: Option<serde_json::Value> = cache.get_json("feed:1:absent").await?;
    assert_eq!(missing, None);

    // Non-JSON bytes surface as a decode error rather than a panic.
    cache.set("verify:raw", b"not json", ttl).await?;
    let decoded: Result<Option<serde_json::Value>, CacheError> = cache.get_json("verify:raw").await;
    assert!(matches!(decoded, Err(CacheError::Serde(_))));

    // The trait's byte-level delete is what the helper-heavy paths use.
    cache.delete("feed:1:next").await?;
    assert!(cache.get_at("feed:1:next", Instant::now())?.is_none());
    Ok(())
}
