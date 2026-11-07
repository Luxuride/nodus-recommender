//! `refresh_account` — P2/P3, the recent public posts of one tracked account.
//!
//! The queue names a local `account.id`, never a user (D20): two people who follow
//! the same author must not produce two copies of that author's posts. That is also
//! why the fetcher spends a token from the host's pool rather than a subject's own
//! consent — and why it may only read public statuses. A pooled token rotates, and
//! a private read has to be made with the owner's token.
//!
//! Three properties are deliberate:
//!
//! * **Public and unlisted only.** The `status` table's `CHECK` constraint admits
//!   nothing else, and [`fetch::map_status`] drops the rest before the write.
//! * **The page walk is bounded.** Ranking needs recent posts, so [`RefreshConfig`]
//!   stops at three pages of twenty; a prolific account is truncated rather than
//!   allowed to spend the host's budget on one author.
//! * **`last_fetched_at` is stamped even for an empty page.** The refresh sweep
//!   reads that column to schedule the next tick; an account that posted nothing
//!   would otherwise be fetched on every round.
//!
//! [`RefreshConfig`] grew from [`SyncConfig`](super::SyncConfig), but it carries
//! only the pacing a status read needs: a page size, a page cap and the interval
//! between requests. The per-host request budget belongs to the P0 crawl; this
//! handler paces itself and hands a 429 back to the queue with the instance's own
//! `Retry-After`.

use core::future::Future;
use core::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use nodus_store::crypto::TokenSealer;
use nodus_store::models::Job;
use sqlx::PgPool;

use super::{Handler, JobError};

mod fetch;

#[cfg(test)]
mod tests;

/// The longest pause the config builds between two pages.
///
/// `SYNC_MAX_RPS_PER_HOST` is a float, and `1 / rps` overflows [`Duration`] for an
/// absurdly small value; a day is far beyond any useful crawl pace and keeps the
/// constructor panic-free.
const MAX_REQUEST_INTERVAL: Duration = Duration::from_secs(86_400);

/// How a status refresh behaves: page size, page cap and request spacing.
///
/// The cap is the quality boundary from `docs/04-ingestion.md`: the ranker only
/// scores recent posts, and an account that posted a hundred times today is still
/// represented by its most recent three pages.
#[derive(Debug, Clone)]
pub struct RefreshConfig {
    /// The sealing key, shared with `nodus-feeds`.
    pub sealer: Arc<TokenSealer>,
    /// Statuses per request; the default is one sweep's worth of a normal account.
    pub page_size: u16,
    /// Hard cap on pages per run.
    pub max_pages: usize,
    /// Pause between two pages, derived from `SYNC_MAX_RPS_PER_HOST`.
    pub request_interval: Duration,
    /// Scheme used to reach an instance: `https`, except for a development
    /// instance that serves plain HTTP on localhost.
    pub scheme: String,
}

impl RefreshConfig {
    /// A config sized for the post feed: 20 statuses, at most 3 pages.
    #[must_use]
    pub fn new(sealer: Arc<TokenSealer>, max_rps: f64) -> Self {
        let rps = if max_rps > 0.0 { max_rps } else { 1.0 };
        let interval = (1.0 / rps).min(MAX_REQUEST_INTERVAL.as_secs_f64());
        Self {
            sealer,
            page_size: 20,
            max_pages: 3,
            request_interval: Duration::from_secs_f64(interval),
            scheme: "https".to_owned(),
        }
    }

    /// Override the page size, for an instance whose cap is lower than the default.
    #[must_use]
    pub const fn with_page_size(mut self, page_size: u16) -> Self {
        self.page_size = page_size;
        self
    }

    /// Talk to instances over `scheme` instead of `https`.
    ///
    /// Only a development instance serving plain HTTP needs this. It is
    /// process-wide on purpose: a scheme that could be chosen per user, or carried
    /// on a job, would be a way to point our pooled token at an attacker's server.
    #[must_use]
    pub fn with_scheme(mut self, scheme: impl Into<String>) -> Self {
        self.scheme = scheme.into();
        self
    }
}

/// The P2/P3 handler.
#[derive(Debug)]
pub struct RefreshAccount {
    pool: PgPool,
    config: RefreshConfig,
}

impl RefreshAccount {
    /// A handler bound to one pool and one pacing config.
    #[must_use]
    pub fn new(pool: PgPool, config: RefreshConfig) -> Self {
        Self { pool, config }
    }
}

impl Handler for RefreshAccount {
    fn kind(&self) -> &'static str {
        "refresh_account"
    }

    fn run<'a>(
        &'a self,
        job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move {
            if job.subject_kind != "account" {
                // The id below is an `account.id`; a user or instance id would
                // resolve to a different row or none, and retrying cannot fix it.
                return Err(JobError::permanent(format!(
                    "refresh_account expects an `account` subject, got `{}`",
                    job.subject_kind
                )));
            }
            let Some(raw) = job.subject_id.as_deref() else {
                return Err(JobError::permanent("refresh_account has no subject_id"));
            };
            let account_id = raw.parse::<i64>().map_err(|error| {
                JobError::permanent(format!("subject_id `{raw}` is not an account id: {error}"))
            })?;
            fetch::run(&self.pool, &self.config, account_id).await?;
            Ok(())
        })
    }
}
