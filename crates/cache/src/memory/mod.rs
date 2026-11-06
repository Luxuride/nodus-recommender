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
//!
//! The directory follows the three traits: [`budget`] is the token bucket,
//! [`lock`] the TTL'd exclusion, [`cache`] the byte cache, and each re-export in
//! this file is the name the rest of the crate and its callers already use.
//! [`poisoned`] is the one piece they share — a lock poisoned by a panicking
//! thread is reported as an unavailable backend rather than as a cache miss.

use crate::cache::CacheError;

mod budget;
mod cache;
mod lock;

pub use budget::InMemoryBudget;
pub use cache::InMemoryCache;
pub use lock::InMemoryLock;

/// A poisoned lock means a thread panicked while holding it; the state may be
/// arbitrary, so the only safe answer is to report the backend unavailable.
pub(super) fn poisoned(what: &str) -> CacheError {
    CacheError::Unavailable(format!("{what} lock poisoned"))
}

#[cfg(test)]
mod tests;
