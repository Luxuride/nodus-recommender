//! The maintenance tick: the work nobody asks for but everything depends on.
//!
//! Every other job in the queue is triggered by something a user did — a sync, a
//! follow, a page being served. Three passes are not: recounting follow-through,
//! recomputing similarity for users whose graph moved, and pruning old rows. Without
//! a tick they simply never run, and the feed quietly serves a model that was true a
//! week ago.
//!
//! The tick is deliberately a *producer*, not a handler: it enqueues ordinary jobs
//! and returns, so a maintenance pass that is running when the process restarts is
//! not lost — the row is still there, and `SKIP LOCKED` means a second replica can
//! pick it up.

use std::future::Future;
use std::time::Duration;

use nodus_store::jobs::{self, NewJob};
use nodus_store::similarity;
use sqlx::PgPool;
use tracing::{debug, info, warn};

/// Priorities, in the queue's own vocabulary (P0 is user-visible work).
const RECOMPUTE_PRIORITY: i16 = 3;
const MAINTENANCE_PRIORITY: i16 = 4;

/// How many users one tick may reschedule.
///
/// A tick is a producer, so a long list in one tick only means the queue grows
/// faster than the workers drain it. Bounding it keeps the backlog meaningful.
pub const USERS_PER_TICK: i64 = 200;

/// What one tick enqueued.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// Whether the instance-wide passes were enqueued this tick.
    pub housekeeping: bool,
    /// How many users were rescheduled.
    pub users: usize,
}

impl Report {
    /// Whether the tick had nothing to do.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        !self.housekeeping && self.users == 0
    }
}

/// Enqueues the passes that only a clock asks for.
#[derive(Debug)]
pub struct Maintenance {
    pool: PgPool,
}

impl Maintenance {
    /// Builds the tick against the shared pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Runs one tick.
    ///
    /// # Errors
    ///
    /// Returns the database error that stopped the tick. A partial tick is safe: every
    /// job it enqueued is deduplicated by the queue's partial unique index, so the next
    /// tick simply sees them already queued.
    pub async fn tick(&self) -> Result<Report, sqlx::Error> {
        let mut report = Report::default();

        // Instance-wide passes first: they are cheap to enqueue and every read path
        // depends on the follow-through baseline being present. They carry no subject,
        // so the queue's partial unique index cannot deduplicate them — two `NULL`s
        // never conflict — and the check has to happen here instead.
        for kind in ["recompute_follow_through", "retain"] {
            if !jobs::has_outstanding(&self.pool, kind).await? {
                let job = NewJob::pending(kind, MAINTENANCE_PRIORITY, "none", None);
                let _ = jobs::enqueue(&self.pool, &job).await?;
            }
        }
        report.housekeeping = true;

        let changed = similarity::recently_changed_users(&self.pool, USERS_PER_TICK).await?;
        for user_id in changed {
            let job = NewJob::pending(
                "recompute_similarity",
                RECOMPUTE_PRIORITY,
                "user",
                Some(user_id.to_string()),
            );
            if jobs::enqueue(&self.pool, &job).await?.is_some() {
                report.users += 1;
            }
        }

        debug!(users = report.users, "maintenance tick");
        Ok(report)
    }

    /// Logs the outcome and swallows a failure: a tick that cannot reach the database
    /// must not take the worker down, because the worker is what would drain the
    /// backlog once the database comes back.
    async fn tick_and_log(&self) {
        match self.tick().await {
            Ok(report) if report.is_empty() => {}
            Ok(report) => info!(users = report.users, "maintenance queued work"),
            Err(error) => warn!(%error, "maintenance tick failed"),
        }
    }
}

/// Runs the tick forever, at `interval`, until `shutdown` resolves.
///
/// The first tick happens immediately: a fresh deployment has an empty follow-through
/// table and no reason to wait a full interval for it.
pub async fn run<F>(maintenance: Maintenance, interval: Duration, shutdown: F)
where
    F: Future<Output = ()>,
{
    let mut shutdown = Box::pin(shutdown);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            () = &mut shutdown => {
                debug!("maintenance loop stopped");
                return;
            }
            _ = ticker.tick() => maintenance.tick_and_log().await,
        }
    }
}
