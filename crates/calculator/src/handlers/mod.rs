//! What the calculator does with a claimed job.
//!
//! A handler is a unit of work with a fixed kind, looked up in a registry. The
//! worker knows nothing about the tiers in `docs/04-ingestion.md`; it only knows
//! how to claim, dispatch and record the outcome. That keeps the queue loop
//! testable without an instance to talk to, and it keeps adding a job kind a
//! matter of adding one file and one line.
//!
//! Two rules for implementers:
//!
//! * A handler returns [`JobError::Permanent`] only when retrying cannot help — a
//!   revoked token, a schema the handler does not understand, a deleted subject.
//!   Everything else is [`JobError::Retry`], because the queue already caps attempts.
//! * A handler never holds a database transaction across a network call. Fetch
//!   first, then write in one short transaction, so a slow instance cannot pin a
//!   connection (and cannot trip the calculator's `statement_timeout`).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use nodus_store::models::Job;

mod recompute_follow_through;
mod recompute_similarity;
mod refresh_account;
mod retain;
mod sync_user;

pub use recompute_follow_through::RecomputeFollowThrough;
pub use recompute_similarity::RecomputeSimilarity;
pub use refresh_account::{RefreshAccount, RefreshConfig};
pub use retain::Retain;
pub use sync_user::{SyncConfig, SyncUser};

/// The outcome of one attempt, when it is not success.
#[derive(Debug)]
pub enum JobError {
    /// Try again later. The delay is the handler's own backoff hint; the queue
    /// applies it and gives up once `job.max_attempts` is reached.
    Retry { after: Duration, reason: String },
    /// Do not retry. The job is marked `failed` and its `last_error` explains why.
    Permanent(String),
}

impl JobError {
    /// A retry with a hint expressed in whole seconds, which is what the queue stores.
    #[must_use]
    pub fn retry(after: Duration, reason: impl Into<String>) -> Self {
        Self::Retry {
            after,
            reason: reason.into(),
        }
    }

    /// A permanent failure with an explanation.
    #[must_use]
    pub fn permanent(reason: impl Into<String>) -> Self {
        Self::Permanent(reason.into())
    }

    /// The text stored in `job.last_error`.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Retry { reason, .. } | Self::Permanent(reason) => reason,
        }
    }
}

/// One kind of work.
///
/// The future is boxed rather than `impl Future` so the registry can hold
/// `Arc<dyn Handler>`: a handler is looked up by the job's kind string, and the
/// alternative — a match in the worker — would make the worker grow with every job
/// kind and make testing a handler require a whole worker.
pub trait Handler: std::fmt::Debug + Send + Sync {
    /// The `job.kind` value this handler serves.
    fn kind(&self) -> &'static str;

    /// Perform one attempt.
    fn run<'a>(
        &'a self,
        job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>>;
}

/// A handler whose work is computation rather than waiting.
///
/// The split is not stylistic. Similarity, affinity and model fitting are CPU-bound
/// loops over hundreds of thousands of edges: run inside an async task they would
/// occupy a runtime thread for seconds, starving every socket and timer on the same
/// reactor, and with `M` replicas the timeout budget would depend on what else
/// happened to be in flight. Wrapping them in [`Blocking`] moves the loop to tokio's
/// blocking pool, which is exactly the "many blocking tasks" case it exists for.
///
/// Promise the trait asks for, and the one the worker depends on: `compute` is
/// idempotent. A blocking task cannot be cancelled once started, so a shutdown or a
/// timeout leaves it running to completion; the next attempt must be safe to run
/// anyway.
pub trait Compute: std::fmt::Debug + Send + Sync + 'static {
    /// The `job.kind` value this handler serves.
    fn kind(&self) -> &'static str;

    /// Perform one attempt. Runs on a blocking thread; do not await inside.
    fn compute(&self, job: &Job) -> Result<(), JobError>;
}

/// Adapts a [`Compute`] into a [`Handler`].
#[derive(Debug)]
pub struct Blocking<C: Compute> {
    inner: Arc<C>,
}

impl<C: Compute> Blocking<C> {
    /// Wrap a compute handler.
    pub fn new(inner: C) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}

impl<C: Compute> Handler for Blocking<C> {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    fn run<'a>(
        &'a self,
        job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        let inner = Arc::clone(&self.inner);
        // The job is small and owned; cloning it is cheaper than the lifetime
        // gymnastics that a borrowed `&Job` across a `spawn_blocking` boundary would
        // require (the closure must be `'static`).
        let job = job.clone();
        Box::pin(async move {
            match tokio::task::spawn_blocking(move || inner.compute(&job)).await {
                Ok(result) => result,
                // A panic in the blocking pool is a bug, not a transient failure, but
                // the queue's attempt cap turns it into one either way.
                Err(join_error) => Err(JobError::retry(
                    Duration::from_secs(30),
                    format!("compute task failed: {join_error}"),
                )),
            }
        })
    }
}

/// The handlers this build knows about.
#[derive(Debug, Default)]
pub struct Registry {
    handlers: HashMap<&'static str, Arc<dyn Handler>>,
}

impl Registry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The registry this build actually runs with: the P0 follow crawl, the P2/P3
    /// account refresh, and the similarity/affinity recompute they feed.
    ///
    /// A kind with no handler fails visibly (`no handler registered`) instead of
    /// being retried forever or, worse, completed without doing anything — which is
    /// what lets the roadmap claim "the queue serves these kinds" honestly.
    #[must_use]
    pub fn standard(
        pool: sqlx::PgPool,
        sync: SyncConfig,
        refresh: RefreshConfig,
        retention: nodus_store::retention::RetentionPolicy,
    ) -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(SyncUser::new(pool.clone(), sync)));
        registry.register(Arc::new(RefreshAccount::new(pool.clone(), refresh)));
        registry.register(Arc::new(RecomputeSimilarity::new(pool.clone())));
        registry.register(Arc::new(RecomputeFollowThrough::new(pool.clone())));
        registry.register(Arc::new(Retain::new(pool, retention)));
        registry
    }

    /// Add a handler, replacing any handler for the same kind.
    pub fn register(&mut self, handler: Arc<dyn Handler>) {
        self.handlers.insert(handler.kind(), handler);
    }

    /// Look up the handler for a job kind.
    #[must_use]
    pub fn get(&self, kind: &str) -> Option<Arc<dyn Handler>> {
        self.handlers.get(kind).cloned()
    }

    /// The kinds this registry can serve, sorted.
    #[must_use]
    pub fn kinds(&self) -> Vec<&'static str> {
        let mut kinds: Vec<_> = self.handlers.keys().copied().collect();
        kinds.sort_unstable();
        kinds
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct AlwaysFine;

    impl Handler for AlwaysFine {
        fn kind(&self) -> &'static str {
            "always_fine"
        }

        fn run<'a>(
            &'a self,
            _job: &'a Job,
        ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
            Box::pin(std::future::ready(Ok(())))
        }
    }

    #[test]
    fn a_registered_handler_is_found_by_kind() {
        let mut registry = Registry::new();
        registry.register(Arc::new(AlwaysFine));
        assert!(registry.get("always_fine").is_some());
        assert_eq!(registry.kinds(), vec!["always_fine"]);
    }

    #[test]
    fn an_unknown_kind_is_reported_as_missing() {
        assert!(Registry::new().get("nope").is_none());
    }

    #[test]
    fn a_retry_error_carries_its_hint_and_reason() {
        let error = JobError::retry(Duration::from_secs(30), "rate limited");
        assert_eq!(error.message(), "rate limited");
        match error {
            JobError::Retry { after, .. } => assert_eq!(after, Duration::from_secs(30)),
            JobError::Permanent(_) => panic!("expected a retry"),
        }
    }
}
