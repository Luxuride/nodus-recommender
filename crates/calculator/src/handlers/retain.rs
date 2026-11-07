//! `retain` — P4, the only job whose purpose is to delete.
//!
//! Retention is a maintenance concern, so this handler does the smallest thing that
//! can be resumed: one [`retain_batch`] call, which drains every table at most
//! [`MAX_STATEMENTS_PER_TABLE`] times and reports what it removed. It never loops
//! until the backlog is empty — the tick enqueues another run, and a long-lived
//! handler would hold a lease while the database is doing the most expensive thing
//! it ever does.
//!
//! The policy comes from configuration (`POST_RETENTION_DAYS`,
//! `EVENT_RETENTION_DAYS`), so a wrong number is a config change rather than a code
//! change. The batch size is a constant here because it is a load knob, not a
//! product decision: small enough that a delete never holds locks that the feed path
//! would notice.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use nodus_store::models::Job;
use nodus_store::retention::{self, RetentionPolicy};
use sqlx::PgPool;
use tracing::info;

use super::{Handler, JobError};

/// Rows deleted per statement.
///
/// Big enough that a backlog drains, small enough that the locks a statement takes
/// are released before the feed path waits on them.
pub const BATCH: i64 = 5_000;

/// How long to wait after a database failure.
///
/// Deleting is deferred work; there is no reason to retry quickly, and a struggling
/// database is the one case where waiting is free.
const RETRY_AFTER: Duration = Duration::from_secs(300);

/// Deletes rows past their deadline, one bounded pass per job.
#[derive(Debug)]
pub struct Retain {
    pool: PgPool,
    policy: RetentionPolicy,
}

impl Retain {
    /// Builds the handler with the configured retention windows.
    #[must_use]
    pub const fn new(pool: PgPool, policy: RetentionPolicy) -> Self {
        Self { pool, policy }
    }
}

impl Handler for Retain {
    fn kind(&self) -> &'static str {
        "retain"
    }

    fn run<'a>(
        &'a self,
        job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move {
            // Retention is instance-wide. A subject would suggest a per-user window,
            // which the schema does not have and the privacy page does not promise.
            if let Some(subject) = job.subject_id.as_deref() {
                return Err(JobError::permanent(format!(
                    "retain is instance-wide, but the job names `{subject}`"
                )));
            }

            let report = retention::retain_batch(&self.pool, &self.policy, BATCH)
                .await
                .map_err(|error| {
                    JobError::retry(RETRY_AFTER, format!("retention pass failed: {error}"))
                })?;

            info!(
                statuses = report.statuses,
                interactions = report.interactions,
                recommendations = report.recommendations,
                jobs = report.jobs,
                batches = report.batches,
                "retention pass finished"
            );
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{BATCH, Retain};
    use crate::handlers::{Handler, JobError};
    use nodus_store::models::Job;
    use nodus_store::retention::RetentionPolicy;
    use serde_json::json;
    use time::OffsetDateTime;

    /// A job shaped like the one the maintenance tick enqueues.
    fn job(subject_id: Option<&str>) -> Job {
        Job {
            id: 1,
            kind: "retain".to_owned(),
            priority: 4,
            subject_kind: "instance".to_owned(),
            subject_id: subject_id.map(str::to_owned),
            payload: json!({}),
            status: "running".to_owned(),
            attempts: 1,
            max_attempts: 3,
            run_after: OffsetDateTime::UNIX_EPOCH,
            locked_at: None,
            locked_by: None,
            last_error: None,
        }
    }

    // A lazy pool still needs a Tokio context: it spawns the pool's background
    // tasks even though it never connects.
    #[tokio::test]
    async fn the_handler_answers_to_the_kind_the_tick_enqueues() {
        let handler = Retain::new(
            sqlx::PgPool::connect_lazy("postgres://unused").expect("a lazy pool parses any URL"),
            RetentionPolicy::default(),
        );
        assert_eq!(handler.kind(), "retain");
        assert_eq!(BATCH, 5_000, "the batch size is a load knob, not a secret");
    }

    #[tokio::test]
    async fn a_job_that_names_a_subject_is_permanent() {
        let handler = Retain::new(
            sqlx::PgPool::connect_lazy("postgres://unused").expect("a lazy pool parses any URL"),
            RetentionPolicy::default(),
        );
        let job = job(Some("42"));
        match handler.run(&job).await {
            Err(JobError::Permanent(reason)) => {
                assert!(reason.contains("instance-wide"), "reason was {reason}");
            }
            other => panic!("expected a permanent failure, got {other:?}"),
        }
    }
}
