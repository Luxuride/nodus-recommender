//! Per-instance request budgets.
//!
//! Mastodon rate limits are the hard constraint on ingestion: an instance that
//! decides we are abusive blocks the whole egress IP, and there is no recourse
//! but waiting. The crawler therefore polices itself before the instance has to.
//!
//! The policy this trait answers is deliberately conservative: at most **one
//! request per second per host, with a burst of five**, and never more than
//! **50% of the remaining budget the instance last reported** in its
//! `X-RateLimit-Remaining` header. The second half cannot live inside
//! [`Budget::acquire`] because the header is only known to the caller that just
//! read it — the crawler applies the fraction on top of a `true` answer here.
//! Two replicas sharing the first half is the point: each one's local view would
//! otherwise multiply the real request rate by the replica count.
//!
//! Two levels are tracked, because D23 makes a token, not a host, the thing that
//! is spent: the bucket the caller names (the crawler passes a token fingerprint,
//! so a drained token does not silence its siblings) and a host-wide hold that a
//! `429` sets. The hold is what an instance's own `Retry-After` becomes, and it is
//! shared so the next job — possibly on another replica — does not walk into the
//! same refusal.
//!
//! A `false` answer is not a failure. It means "not now": the caller should
//! reschedule the job for when the bucket has refilled, not spin or treat the
//! instance as broken.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::CacheError;

/// Decides whether one more request to `host` may be made right now.
///
/// Implementations are shared between tasks, so the answer must be atomic with
/// respect to concurrent callers: two calls that observe the same last token
/// must not both succeed. The trait is backend-agnostic — in-process buckets and
/// a shared Redis counter both fit — but the policy it encodes is always the one
/// above: 1 request/second sustained, burst 5, and the caller's 50% ceiling on
/// the instance's reported remaining budget.
pub trait Budget: std::fmt::Debug + Send + Sync {
    /// Take one request from `host`, charged to `bucket`, returning whether it may
    /// proceed.
    ///
    /// `bucket` names the token being spent (the crawler uses its fingerprint);
    /// `host` is the instance, whose hold applies to every bucket on it.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] when the shared store cannot be
    /// reached. The caller should then treat the answer as `false` for the
    /// current attempt rather than spend the request unaccounted for: an
    /// unreachable budget is a reason to slow down, never to spend freely.
    fn acquire<'a>(
        &'a self,
        host: &'a str,
        bucket: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, CacheError>> + Send + 'a>>;

    /// Refuse every bucket on `host` until `hold` has elapsed.
    ///
    /// This is `Retry-After` turned into state instead of a sleep: the instance
    /// has told one caller to wait, and the same wait applies to every token and
    /// every replica. The delay is capped by the implementation so a hostile
    /// header cannot park a host forever.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] when the shared store cannot be
    /// reached; the caller logs it and keeps the local `Retry-After` behaviour.
    fn hold<'a>(
        &'a self,
        host: &'a str,
        hold: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>>;

    /// How long to wait before asking [`Budget::acquire`] again after a refusal.
    ///
    /// A bucket refills continuously, so the honest answer is "until one token
    /// accrues"; this is what the caller uses as the job's retry delay, which is
    /// why it must not be zero for a spent bucket. Implementations that cannot see
    /// their own state cheaply return the sustained interval instead.
    fn retry_hint(&self, host: &str, bucket: &str) -> Duration;
}
