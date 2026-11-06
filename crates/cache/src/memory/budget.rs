//! The in-process token bucket behind [`Budget`](crate::budget::Budget).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::budget::Budget;
use crate::cache::CacheError;

use super::poisoned;

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
    pub(super) fn hint_at(&self, host: &str, bucket: &str, now: Instant) -> Duration {
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
