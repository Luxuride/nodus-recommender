//! The claim → dispatch → record loop.
//!
//! One loop per replica, entirely asynchronous. Claiming, dispatch, the idle
//! sleep, the lease reaper and shutdown are all futures on the tokio runtime:
//! a batch of eight in-flight jobs costs eight small tasks, not eight threads.
//!
//! The loop owns three invariants:
//!
//! * **Every claimed job gets an outcome.** Success, a retry hint or a permanent
//!   failure is always written back. A job that silently disappears from `running`
//!   is the one state the queue cannot recover from.
//! * **A lease is a promise.** If a handler runs past its lease the job is re-queued
//!   and a second replica may run it, so every handler must be idempotent — which is
//!   why handlers recompute rather than accumulate.
//! * **Shutdown drains, not aborts.** On `SIGTERM` the loop stops claiming, finishes
//!   what it holds, and exits; Kubernetes then does not need to wait for lease
//!   expiry to know the replica is gone.

use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use nodus_store::jobs;
use nodus_store::models::Job;
use sqlx::PgPool;
use time::Duration as SqlDuration;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep, timeout};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::handlers::{JobError, Registry};

/// Runs jobs until shutdown is signalled.
///
/// `Arc` because each running job needs a handle to record its own outcome: a
/// dispatch that is cancelled with the batch would leave the row `running` until
/// the lease expired, which is exactly the latency the reaper is a backstop for.
#[derive(Debug, Clone)]
pub struct Worker {
    pool: PgPool,
    registry: Arc<Registry>,
    config: Arc<Config>,
    reaper: Option<Arc<JoinHandle<()>>>,
}

impl Worker {
    /// Assemble a worker. The pool is shared with anything else the replica runs.
    #[must_use]
    pub fn new(pool: PgPool, registry: Registry, config: Config) -> Self {
        Self {
            pool,
            registry: Arc::new(registry),
            config: Arc::new(config),
            reaper: None,
        }
    }

    /// How long one attempt may run before it is treated as a failure.
    ///
    /// Half the lease: a job that outlives the lease is already claimable by another
    /// replica, so finishing it late is wasted work twice over.
    #[must_use]
    pub fn handler_timeout(&self) -> Duration {
        self.config.lease / 2
    }

    /// Start the background lease reaper.
    ///
    /// Runs at a third of the lease interval so a dead replica's jobs are picked up
    /// within roughly one lease plus a tick, and its own failure is logged rather
    /// than panicking the worker.
    pub fn spawn_reaper(&mut self) {
        let pool = self.pool.clone();
        let lease = self.config.lease;
        let period = lease / 3;
        // The store speaks `time::Duration` because that is what sqlx encodes; the
        // loop speaks `std::time::Duration` because that is what tokio timers take.
        let lease_sql = sql_duration(lease);
        self.reaper = Some(Arc::new(tokio::spawn(async move {
            let mut ticker = interval(period);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match jobs::reap_expired_leases(&pool, lease_sql).await {
                    Ok(0) => {}
                    Ok(reaped) => warn!(reaped, "re-queued jobs whose lease expired"),
                    Err(error) => error!(%error, "lease reaper failed"),
                }
            }
        })));
    }

    /// Claim and run jobs until `shutdown` resolves.
    ///
    /// # Errors
    ///
    /// Returns the first unrecoverable queue error — a database that has gone away
    /// is not something to spin on, and the orchestrator should restart the replica
    /// with backoff instead.
    pub async fn run<F>(mut self, shutdown: F) -> Result<(), sqlx::Error>
    where
        F: std::future::Future<Output = ()>,
    {
        self.spawn_reaper();
        info!(
            worker = %self.config.worker_id,
            batch = self.config.batch_size,
            lease_s = self.config.lease.as_secs(),
            kinds = ?self.config.kinds,
            "calculator loop started"
        );

        let result = self.loop_until(shutdown).await;
        if let Some(reaper) = self.reaper.take() {
            reaper.abort();
        }
        result
    }

    async fn loop_until<F>(&mut self, shutdown: F) -> Result<(), sqlx::Error>
    where
        F: std::future::Future<Output = ()>,
    {
        tokio::pin!(shutdown);

        loop {
            let claimed = tokio::select! {
                () = &mut shutdown => {
                    info!("shutdown requested, no new claims");
                    return Ok(());
                }
                claimed = jobs::claim_kinds(
                    &self.pool,
                    &self.config.worker_id,
                    self.config.batch_size,
                    self.config.kinds.as_deref(),
                ) => claimed?,
            };

            if claimed.is_empty() {
                tokio::select! {
                    () = &mut shutdown => return Ok(()),
                    () = sleep(self.config.poll_interval) => {}
                }
                continue;
            }

            // Concurrent, not parallel: these handlers spend their time waiting on an
            // instance, so the reactor should hold all of them at once. Anything that
            // computes instead of waits goes through `handlers::Blocking`, which moves
            // it to `spawn_blocking` — otherwise a recompute would stall this reactor
            // for as long as it takes, and every other job in flight with it.
            let mut handles = Vec::with_capacity(claimed.len());
            for job in claimed {
                let worker = self.clone();
                handles.push(tokio::spawn(async move { worker.dispatch(job).await }));
            }

            let finished = join_all(handles)
                .await
                .into_iter()
                .filter(|result| matches!(result, Ok(true)))
                .count();
            debug!(finished, "batch drained");
        }
    }

    async fn dispatch(&self, job: Job) -> bool {
        let Some(handler) = self.registry.get(&job.kind) else {
            // An unregistered kind is a deployment mismatch (an older image, or a
            // producer that got ahead of its consumer). Permanent, because retrying
            // the same binary cannot help.
            let reason = format!("no handler registered for kind `{}`", job.kind);
            warn!(job = job.id, kind = %job.kind, reason, "unregistered job kind");
            return self
                .record_failure(&job, &JobError::permanent(reason))
                .await;
        };

        let started = Instant::now();
        let outcome = timeout(self.handler_timeout(), handler.run(&job)).await;

        match outcome {
            Ok(Ok(())) => {
                if let Err(error) = jobs::complete(&self.pool, job.id).await {
                    error!(job = job.id, %error, "failed to record success");
                    return false;
                }
                info!(job = job.id, kind = %job.kind, ms = started.elapsed().as_millis(), "done");
                true
            }
            Ok(Err(failure)) => self.record_failure(&job, &failure).await,
            Err(_elapsed) => {
                let failure = JobError::retry(
                    Duration::from_secs(60),
                    format!("handler exceeded {}s", self.handler_timeout().as_secs()),
                );
                self.record_failure(&job, &failure).await
            }
        }
    }

    async fn record_failure(&self, job: &Job, failure: &JobError) -> bool {
        let (after, permanent) = match failure {
            JobError::Retry { after, .. } => (*after, false),
            JobError::Permanent(_) => (Duration::ZERO, true),
        };
        if let Err(error) =
            jobs::fail(&self.pool, job.id, failure.message(), sql_duration(after)).await
        {
            error!(job = job.id, %error, "failed to record failure");
            return false;
        }
        if permanent {
            warn!(job = job.id, kind = %job.kind, reason = failure.message(), "failed permanently");
        } else {
            info!(
                job = job.id,
                kind = %job.kind,
                retry_in_s = after.as_secs(),
                reason = failure.message(),
                attempt = job.attempts,
                "will retry"
            );
        }
        // A retry is a normal outcome, not a lost job.
        true
    }
}

/// Whole seconds, saturating: both queue durations are minute-scale in practice, and a
/// saturating conversion keeps an absurd configuration from panicking at runtime.
fn sql_duration(duration: Duration) -> SqlDuration {
    SqlDuration::seconds(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(lease_seconds: u64) -> Config {
        Config {
            database_url: "postgres://unused".to_owned(),
            worker_id: "test:1".to_owned(),
            pool_size: 1,
            batch_size: 1,
            poll_interval: Duration::from_millis(1),
            lease: Duration::from_secs(lease_seconds),
            replicas: 1,
            redis_url: None,
            kinds: None,
            max_rps_per_host: 1.0,
            budget_fraction: 0.5,
            event_retention_days: 90,
            post_retention_days: 14,
        }
    }

    #[tokio::test]
    async fn the_handler_timeout_is_half_the_lease() {
        // A lazy pool still installs a background task, which needs a runtime — hence
        // the tokio test rather than a plain one.
        let pool = PgPool::connect_lazy("postgres://unused").expect("lazy pool");
        let worker = Worker::new(pool, Registry::new(), config(600));
        assert_eq!(worker.handler_timeout(), Duration::from_secs(300));
    }
}
