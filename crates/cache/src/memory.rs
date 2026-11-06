//! In-process implementations of all three traits.
//!
//! These exist for two reasons. Development and the single-replica deployment
//! must work with no Redis at all — the service refuses to start with more than
//! one replica when the state is per-process, but one replica is a supported
//! configuration and should not need another daemon. And tests need budgets,
//! locks and caches whose clock can be moved without sleeping.
//!
//! A `Mutex<HashMap<..>>` is the right amount of machinery here: contention is
//! one lock per request, held only long enough to update a counter or copy a
//! byte vector, so a sharded map or an async mutex would be complexity without a
//! measurable gain. Every method therefore completes without awaiting while
//! holding the lock; the returned futures are wrappers, not I/O.
//!
//! The budget is a token bucket rather than a per-second counter. A counter
//! would refuse five requests at the end of one second and then admit five more
//! at the start of the next — the exact burst-of-ten the policy is meant to
//! prevent — while a bucket refills continuously and still allows the configured
//! burst from cold. Clock injection is explicit (`_at` methods) instead of a
//! trait object so tests read as "at t, then at t + 1 s" and cannot accidentally
//! depend on wall time.
//!
//! Buckets are keyed by `(host, bucket)` — the crawler passes a token fingerprint
//! as the bucket so one drained token does not silence its siblings — while a hold
//! is keyed by host alone, because a `429` is the instance refusing all of them.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::budget::Budget;
use crate::cache::{Cache, CacheError};
use crate::lock::{Lock, LockGuard};

/// A poisoned lock means a thread panicked while holding it; the state may be
/// arbitrary, so the only safe answer is to report the backend unavailable.
fn poisoned(what: &str) -> CacheError {
    CacheError::Unavailable(format!("{what} lock poisoned"))
}

/// Token-bucket request budget, one bucket per `(host, token)`.
///
/// The bucket starts full, so a cold start may spend `burst` requests
/// immediately; refill is continuous at `rate_per_second`. This is the exact
/// policy described on [`Budget`], minus the 50%-of-reported-remaining rule,
/// which the caller applies from response headers. A host-wide hold (the
/// instance's own `Retry-After`) refuses every bucket on that host until it
/// expires.
#[derive(Debug)]
pub struct InMemoryBudget {
    rate_per_second: f64,
    burst: f64,
    buckets: Mutex<HashMap<String, Bucket>>,
    holds: Mutex<HashMap<String, Instant>>,
}

/// The longest a single `Retry-After` may park a host, so a broken header cannot
/// stop the crawl indefinitely.
const MAX_HOLD: Duration = Duration::from_secs(86_400);

fn bucket_key(host: &str, bucket: &str) -> String {
    format!("{host}:{bucket}")
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl InMemoryBudget {
    /// Create a budget allowing `burst` requests immediately and then
    /// `rate_per_second` requests per second per host.
    ///
    /// A rate of zero allows the initial burst and nothing after it; a burst of
    /// zero refuses everything. Both are valid inputs — they describe an
    /// instance we have decided to stop crawling — not configuration errors.
    #[must_use]
    pub fn new(rate_per_second: u32, burst: u32) -> Self {
        Self {
            rate_per_second: f64::from(rate_per_second),
            burst: f64::from(burst),
            buckets: Mutex::new(HashMap::new()),
            holds: Mutex::new(HashMap::new()),
        }
    }

    /// Take one request from `bucket` on `host` at `now`.
    ///
    /// The explicit instant is the test seam: production code goes through
    /// [`Budget::acquire`], which passes [`Instant::now`]. Time moving backwards
    /// (only possible when a test injects it) refills nothing. A live hold on
    /// `host` refuses before the bucket is touched, so the refused request costs
    /// nothing when the hold ends.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the bucket or hold map is poisoned.
    pub fn acquire_at(&self, host: &str, bucket: &str, now: Instant) -> Result<bool, CacheError> {
        let held = self
            .holds
            .lock()
            .map_err(|_| poisoned("budget hold"))?
            .get(host)
            .copied()
            .is_some_and(|until| until > now);
        if held {
            return Ok(false);
        }

        let mut buckets = self.buckets.lock().map_err(|_| poisoned("budget"))?;
        let bucket = buckets.entry(bucket_key(host, bucket)).or_insert(Bucket {
            tokens: self.burst,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        let tokens = (bucket.tokens + elapsed * self.rate_per_second).min(self.burst);
        let allowed = tokens >= 1.0;
        bucket.tokens = if allowed { tokens - 1.0 } else { tokens };
        bucket.last = now;
        Ok(allowed)
    }

    /// Park every bucket on `host` until `now + hold`.
    ///
    /// A hold is a floor, not a new clock: a shorter `Retry-After` arriving while
    /// a longer hold is live must not cut it short. The explicit instant is the
    /// test seam.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the hold map is poisoned.
    pub fn hold_at(&self, host: &str, hold: Duration, now: Instant) -> Result<(), CacheError> {
        let until = now
            .checked_add(hold.min(MAX_HOLD))
            .unwrap_or(now + MAX_HOLD);
        let mut holds = self.holds.lock().map_err(|_| poisoned("budget hold"))?;
        let entry = holds.entry(host.to_owned()).or_insert(until);
        if *entry < until {
            *entry = until;
        }
        Ok(())
    }

    /// When one more token will have accrued for `bucket`, or when the host's
    /// hold ends — whichever is later in effect.
    fn hint_at(&self, host: &str, bucket: &str, now: Instant) -> Duration {
        let held_until = self
            .holds
            .lock()
            .ok()
            .and_then(|holds| holds.get(host).copied())
            .filter(|until| *until > now);
        if let Some(until) = held_until {
            return until - now;
        }

        let Ok(buckets) = self.buckets.lock() else {
            return self.pace(1.0);
        };
        let Some(state) = buckets.get(&bucket_key(host, bucket)) else {
            return Duration::ZERO;
        };
        let elapsed = now.saturating_duration_since(state.last).as_secs_f64();
        let tokens = (state.tokens + elapsed * self.rate_per_second).min(self.burst);
        if tokens >= 1.0 {
            return Duration::ZERO;
        }
        self.pace(1.0 - tokens)
    }

    /// How long `tokens` worth of requests take to accrue at the configured rate.
    fn pace(&self, tokens: f64) -> Duration {
        if self.rate_per_second <= 0.0 {
            // A zero rate never refills; report the policy's floor so callers back
            // off by a second instead of spinning on a zero delay.
            return Duration::from_secs(1);
        }
        Duration::from_secs_f64(tokens / self.rate_per_second)
    }
}

impl Budget for InMemoryBudget {
    fn acquire<'a>(
        &'a self,
        host: &'a str,
        bucket: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, CacheError>> + Send + 'a>> {
        Box::pin(async move { self.acquire_at(host, bucket, Instant::now()) })
    }

    fn hold<'a>(
        &'a self,
        host: &'a str,
        hold: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        Box::pin(async move { self.hold_at(host, hold, Instant::now()) })
    }

    fn retry_hint(&self, host: &str, bucket: &str) -> Duration {
        self.hint_at(host, bucket, Instant::now())
    }
}

/// In-process exclusive locks with TTL expiry.
///
/// Only correct within one process. More than one replica needs the Redis
/// backend, because two independent maps cannot see each other's holds.
#[derive(Debug, Default)]
pub struct InMemoryLock {
    held: Mutex<HashMap<String, Held>>,
}

#[derive(Debug, Clone)]
struct Held {
    owner: String,
    /// `None` when the TTL overflowed an `Instant`; such a hold never expires.
    expires_at: Option<Instant>,
}

impl Held {
    fn is_live(&self, now: Instant) -> bool {
        self.expires_at.is_none_or(|expires_at| expires_at > now)
    }
}

/// Distinguishes two acquisitions in the same process, and two processes on the
/// same machine, without pulling in a UUID dependency. Only the owner string has
/// to be unique, not globally sortable.
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

fn next_owner() -> String {
    let sequence = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{sequence}", std::process::id())
}

impl InMemoryLock {
    /// Create an empty lock table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn table(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Held>>, CacheError> {
        self.held.lock().map_err(|_| poisoned("lock"))
    }

    /// Try to take `name` for `ttl` at `now`.
    ///
    /// An expired hold is treated as absent, which is the lease semantics the
    /// trait promises. The explicit instant is the test seam; production code
    /// goes through [`Lock::acquire`].
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the lock table is poisoned.
    pub fn acquire_at(
        &self,
        name: &str,
        ttl: Duration,
        now: Instant,
    ) -> Result<Option<LockGuard>, CacheError> {
        let mut held = self.table()?;
        if held.get(name).is_some_and(|entry| entry.is_live(now)) {
            return Ok(None);
        }
        let owner = next_owner();
        held.insert(
            name.to_owned(),
            Held {
                owner: owner.clone(),
                expires_at: now.checked_add(ttl),
            },
        );
        Ok(Some(LockGuard::new(name, owner)))
    }

    /// Release `guard`, but only if its owner token is still the stored one.
    ///
    /// A stale guard — one whose TTL expired and was re-acquired — leaves the
    /// new hold untouched. This is the same compare-and-delete the shared
    /// backend performs, expressed as a single map operation.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the lock table is poisoned.
    pub fn unlock(&self, guard: &LockGuard) -> Result<(), CacheError> {
        let mut held = self.table()?;
        if held
            .get(guard.name())
            .is_some_and(|entry| entry.owner == guard.owner())
        {
            held.remove(guard.name());
        }
        Ok(())
    }
}

impl Lock for InMemoryLock {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        ttl: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<LockGuard>, CacheError>> + Send + 'a>> {
        Box::pin(async move { self.acquire_at(name, ttl, Instant::now()) })
    }

    fn release<'a>(
        &'a self,
        guard: &'a LockGuard,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        Box::pin(async move { self.unlock(guard) })
    }
}

/// In-process byte cache with TTLs.
///
/// Unlike the budget and the lock this is a pure convenience: a single replica
/// could keep these bytes in a field, and nothing here is larger than the feed
/// and token payloads the service already handles.
#[derive(Debug, Default)]
pub struct InMemoryCache {
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug, Clone)]
struct Entry {
    value: Vec<u8>,
    /// `None` when the TTL overflowed an `Instant`; such an entry never expires.
    expires_at: Option<Instant>,
}

impl Entry {
    fn is_live(&self, now: Instant) -> bool {
        self.expires_at.is_none_or(|expires_at| expires_at > now)
    }
}

impl InMemoryCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn table(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Entry>>, CacheError> {
        self.entries.lock().map_err(|_| poisoned("cache"))
    }

    /// Read `key` at `now`, dropping it first if it has expired.
    ///
    /// The explicit instant is the test seam; production code goes through
    /// [`Cache::get`].
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the cache map is poisoned.
    pub fn get_at(&self, key: &str, now: Instant) -> Result<Option<Vec<u8>>, CacheError> {
        let mut entries = self.table()?;
        let expired = entries.get(key).is_some_and(|entry| !entry.is_live(now));
        if expired {
            entries.remove(key);
            return Ok(None);
        }
        Ok(entries.get(key).map(|entry| entry.value.clone()))
    }

    /// Write `value` under `key` until `now + ttl`.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the cache map is poisoned.
    pub fn set_at(
        &self,
        key: &str,
        value: &[u8],
        ttl: Duration,
        now: Instant,
    ) -> Result<(), CacheError> {
        let mut entries = self.table()?;
        entries.insert(
            key.to_owned(),
            Entry {
                value: value.to_vec(),
                expires_at: now.checked_add(ttl),
            },
        );
        Ok(())
    }

    /// Forget `key` immediately.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the cache map is poisoned.
    pub fn remove(&self, key: &str) -> Result<(), CacheError> {
        let mut entries = self.table()?;
        entries.remove(key);
        Ok(())
    }
}

impl Cache for InMemoryCache {
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, CacheError>> + Send + 'a>> {
        Box::pin(async move { self.get_at(key, Instant::now()) })
    }

    fn set<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        Box::pin(async move { self.set_at(key, value, ttl, Instant::now()) })
    }

    fn delete<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        Box::pin(async move { self.remove(key) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let decoded: Result<Option<serde_json::Value>, CacheError> =
            cache.get_json("verify:raw").await;
        assert!(matches!(decoded, Err(CacheError::Serde(_))));

        // The trait's byte-level delete is what the helper-heavy paths use.
        cache.delete("feed:1:next").await?;
        assert!(cache.get_at("feed:1:next", Instant::now())?.is_none());
        Ok(())
    }
}
