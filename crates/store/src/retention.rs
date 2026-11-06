//! Batched retention: deleting rows past their deadline without stalling the service.
//!
//! Deletion is the riskiest SQL we run, for two reasons. A single mass `DELETE` holds
//! row locks for as long as it scans, so every writer on the feed path stacks up behind
//! it; and a statement that is interrupted rolls back completely, so a backlog only ever
//! shrinks by a run that finishes. Both are avoided the same way: every table is drained
//! in small, independently committed passes, each bounded by `LIMIT` over `ctid`.
//!
//! `ctid` instead of the primary key keeps one pass to a single scan of the deadline
//! column plus a row-local delete: the subquery returns at most `batch` physical
//! locations and the delete touches exactly those rows, however wide they are. Passes
//! are idempotent because the predicate is a deadline, never a cursor — a crash halfway
//! leaves the remainder for the next run, and a row inside its window is never touched.
//!
//! Order matters for counting. Children are drained before their parents
//! (`interaction_event` before `recommendation_item`, `recommendation_item` before
//! `status`), because deleting a parent cascades to child rows and a cascaded row is
//! invisible to the child pass — it would disappear without appearing in the report.

use serde::Serialize;
use sqlx::PgPool;

/// How long each kind of row is kept, in days.
///
/// These are the numbers the service configuration carries (`POST_RETENTION_DAYS`,
/// `EVENT_RETENTION_DAYS`) and the values the schema's own defaults were written from.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    /// Days a `status` row is kept.
    ///
    /// A status carries its deadline in `status.expires_at`, stamped on insert, so this
    /// value documents and configures new rows rather than taking part in the delete
    /// predicate: shortening it does not retroactively expire rows already written.
    pub post_days: u32,
    /// Days an `interaction_event` row is kept, measured on `server_ts`.
    ///
    /// Unlike statuses this is evaluated when the delete runs, so lowering it takes
    /// effect on the next pass instead of only for future rows. `server_ts` is our
    /// receive clock; `client_ts` is what the client claims and is not trusted for
    /// retention.
    pub event_days: u32,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            post_days: 14,
            event_days: 90,
        }
    }
}

impl RetentionPolicy {
    /// The event cutoff as a Postgres interval literal, e.g. `"90 days"`.
    ///
    /// Built from a number and a fixed unit, so no user-supplied text ever reaches the
    /// interval parser.
    fn event_interval(self) -> String {
        let days = self.event_days;
        format!("{days} days")
    }
}

/// What one [`retain_batch`] call removed.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct RetentionReport {
    /// `status` rows deleted.
    pub statuses: u64,
    /// `interaction_event` rows deleted.
    pub interactions: u64,
    /// `recommendation_item` rows deleted.
    pub recommendations: u64,
    /// Finished `job` rows deleted.
    pub jobs: u64,
    /// Statements executed, across all four tables.
    pub batches: u32,
}

/// Upper bound on statements for one table within a single call.
///
/// One call must be bounded work. Four tables at 25 statements is at most 100 statements
/// per call, which keeps a tick's cost predictable even when the tables hold months of
/// expired rows; splitting the budget per table means a telemetry backlog cannot starve
/// the job cleanup, and the job cleanup cannot starve telemetry. Whatever is left is
/// picked up by the next tick, so the backlog still drains.
const MAX_STATEMENTS_PER_TABLE: u32 = 25;

/// One bounded pass over expired telemetry.
const DELETE_EVENTS: &str = "DELETE FROM interaction_event WHERE ctid IN ( \
     SELECT ctid FROM interaction_event \
     WHERE server_ts < now() - $1::text::interval LIMIT $2)";

/// One bounded pass over expired posts.
const DELETE_STATUSES: &str = "DELETE FROM status WHERE ctid IN ( \
     SELECT ctid FROM status WHERE expires_at < now() LIMIT $1)";

/// One bounded pass over served items past their attribution window.
const DELETE_RECOMMENDATIONS: &str = "DELETE FROM recommendation_item WHERE ctid IN ( \
     SELECT ctid FROM recommendation_item WHERE expires_at < now() LIMIT $1)";

/// One bounded pass over finished jobs.
///
/// `cancelled` is in the predicate although `job_status_valid` does not allow it yet:
/// the predicate is the intent, and it stays correct if the queue grows that terminal
/// state.
const DELETE_JOBS: &str = "DELETE FROM job WHERE ctid IN ( \
     SELECT ctid FROM job \
     WHERE status IN ('done', 'failed', 'cancelled') \
         AND finished_at < now() - interval '7 days' \
     LIMIT $1)";

/// Whether a pass that just deleted `deleted` rows should be followed by another.
///
/// A short pass is the signal that the deadline-filtered set is exhausted; a full pass
/// means there may be more behind it. A non-positive batch deletes nothing and never
/// terminates a loop by itself, so it is treated as "stop" rather than spinning.
fn should_continue(deleted: u64, batch: i64) -> bool {
    u64::try_from(batch).is_ok_and(|batch| batch > 0 && deleted >= batch)
}

/// Delete what is past its deadline, in `batch`-sized steps.
///
/// Returns what was deleted, not what is left: the caller's job record is a log line,
/// and a count of remaining rows would need a second full scan to produce.
///
/// The call is bounded twice over — `batch` rows per statement, and at most 100
/// statements overall (25 per table) — so it can run alongside serving traffic, and it is
/// safe to re-run at any time. `batch` must be positive for any progress to be made; zero
/// is accepted and does nothing, which keeps a misconfigured schedule from erroring on
/// every tick.
///
/// # Errors
///
/// Propagates [`sqlx::Error`] from the first failing statement. Statements already
/// committed stay committed, which is why the caller can simply retry.
pub async fn retain_batch(
    pool: &PgPool,
    policy: &RetentionPolicy,
    batch: i64,
) -> Result<RetentionReport, sqlx::Error> {
    let mut statements = 0_u32;
    let event_interval = policy.event_interval();

    let interactions = drain(
        pool,
        DELETE_EVENTS,
        Some(event_interval.as_str()),
        batch,
        &mut statements,
    )
    .await?;
    let recommendations = drain(pool, DELETE_RECOMMENDATIONS, None, batch, &mut statements).await?;
    let statuses = drain(pool, DELETE_STATUSES, None, batch, &mut statements).await?;
    let jobs = drain(pool, DELETE_JOBS, None, batch, &mut statements).await?;

    Ok(RetentionReport {
        statuses,
        interactions,
        recommendations,
        jobs,
        batches: statements,
    })
}

/// Drain one table with repeated bounded deletes, charging each statement to the report.
///
/// `interval` is bound as `$1` when the statement takes one, which is why the callers
/// pass the same shape for every table: interval (optional), then `batch`.
async fn drain(
    pool: &PgPool,
    sql: &str,
    interval: Option<&str>,
    batch: i64,
    statements: &mut u32,
) -> Result<u64, sqlx::Error> {
    let mut deleted_total = 0_u64;
    for _ in 0..MAX_STATEMENTS_PER_TABLE {
        let mut query = sqlx::query(sql);
        if let Some(interval) = interval {
            query = query.bind(interval);
        }
        let outcome = query.bind(batch).execute(pool).await?;
        *statements += 1;
        let deleted = outcome.rows_affected();
        deleted_total += deleted;
        if !should_continue(deleted, batch) {
            break;
        }
    }
    Ok(deleted_total)
}

#[cfg(test)]
mod tests {
    use super::{RetentionPolicy, should_continue};

    #[test]
    fn a_full_pass_asks_for_another_one() {
        assert!(should_continue(1_000, 1_000));
        assert!(should_continue(5, 5));
    }

    #[test]
    fn a_short_pass_stops() {
        assert!(!should_continue(999, 1_000));
        assert!(!should_continue(0, 1_000));
    }

    #[test]
    fn a_non_positive_batch_never_loops() {
        assert!(!should_continue(0, 0));
        assert!(!should_continue(10, -5));
        assert!(!should_continue(10, 0));
    }

    #[test]
    fn default_policy_matches_the_documented_windows() {
        let policy = RetentionPolicy::default();
        assert_eq!(policy.post_days, 14);
        assert_eq!(policy.event_days, 90);
        assert_eq!(policy.event_interval(), "90 days");
    }
}
