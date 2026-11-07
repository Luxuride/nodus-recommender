//! `recompute_follow_through` — P3, the evidence behind the lift in D29.
//!
//! The pass is entirely local: it reads `interaction_event` joined to the account
//! items those events belong to, and writes one row per account. No instance traffic,
//! so a retry is cheap and a stale pass is harmless — the next one overwrites it.
//!
//! It is a whole-instance pass rather than a per-user one on purpose. A user's own
//! follow-through says very little ("did this user follow anything this week?"),
//! while the instance-wide baseline is exactly the denominator the ranker needs, and
//! it must be the same number for every request that reads it.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use nodus_store::follow_through;
use nodus_store::models::Job;
use sqlx::PgPool;
use tracing::info;

use super::{Handler, JobError};

/// How long to wait before recounting after a database failure.
///
/// The pass is idempotent and cheap, so a short retry beats waiting for the next
/// scheduled maintenance window.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// Recomputes every account's impression and follow counters.
#[derive(Debug)]
pub struct RecomputeFollowThrough {
    pool: PgPool,
}

impl RecomputeFollowThrough {
    /// Builds the handler against the shared pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl Handler for RecomputeFollowThrough {
    fn kind(&self) -> &'static str {
        "recompute_follow_through"
    }

    fn run<'a>(
        &'a self,
        job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move {
            // The job carries no subject: the counters are per instance, and a
            // subject would suggest a scope this pass does not have.
            if let Some(subject) = job.subject_id.as_deref() {
                return Err(JobError::permanent(format!(
                    "recompute_follow_through is instance-wide, but the job names `{subject}`"
                )));
            }

            let report = follow_through::recompute(&self.pool)
                .await
                .map_err(|error| {
                    JobError::retry(
                        RETRY_AFTER,
                        format!("follow-through recompute failed: {error}"),
                    )
                })?;

            info!(
                accounts = report.accounts,
                pruned = report.pruned,
                expected_rate = ?report.expected_rate,
                "follow-through recomputed"
            );
            Ok(())
        })
    }
}
