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
//! A `false` answer is not a failure. It means "not now": the caller should
//! reschedule the job for when the bucket has refilled, not spin or treat the
//! instance as broken.

use std::future::Future;
use std::pin::Pin;

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
    /// Take one request from `host`'s budget, returning whether it may proceed.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] when the shared store cannot be
    /// reached. The caller should then treat the answer as `false` for the
    /// current attempt rather than spend the request unaccounted for: an
    /// unreachable budget is a reason to slow down, never to spend freely.
    fn acquire<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, CacheError>> + Send + 'a>>;
}
