//! The job queue.
//!
//! Postgres is the queue (decision D9): `SELECT … FOR UPDATE SKIP LOCKED` gives us
//! claimed work without a broker, and the job row can be updated in the same
//! transaction as the data it produces. Two properties are load-bearing:
//!
//! * **Claiming is a single statement** that both selects and marks rows, so two
//!   workers cannot claim the same job and a crashed worker only holds its lease until
//!   [`reap_expired_leases`] runs.
//! * **Enqueue is idempotent per subject.** A partial unique index on
//!   `(kind, subject_kind, subject_id)` for outstanding jobs means that ten requests
//!   for the same user's sync collapse into one job. Priority bumps instead of
//!   duplicating.

use crate::models::Job;
use sqlx::PgPool;
use time::OffsetDateTime;

/// A job to enqueue.
#[derive(Debug, Clone)]
pub struct NewJob {
    /// One of the values allowed by the `job_kind_valid` constraint.
    pub kind: String,
    /// P0 is 0, P4 is 4.
    pub priority: i16,
    /// `user`, `account`, `instance` or `none`.
    pub subject_kind: String,
    /// Identity of the subject, rendered as text so one column serves uuid and bigint
    /// subjects. `None` is allowed but two `None` subjects never conflict.
    pub subject_id: Option<String>,
    pub payload: serde_json::Value,
    /// Earliest start time; `None` means now.
    pub run_after: Option<OffsetDateTime>,
}

impl NewJob {
    /// A P0 job for one user.
    #[must_use]
    pub fn sync_user(user_id: &uuid::Uuid) -> Self {
        Self {
            kind: "sync_user".to_owned(),
            priority: 0,
            subject_kind: "user".to_owned(),
            subject_id: Some(user_id.to_string()),
            payload: serde_json::Value::Null,
            run_after: None,
        }
    }
}

/// Enqueue a job unless an equivalent one is already outstanding.
///
/// Returns the new job id, or `None` when an outstanding job for the same subject
/// already exists. In that case the existing job's priority is raised to the requested
/// one, so a P0 request cannot be starved behind an earlier P4 enqueue.
///
/// # Errors
///
/// Propagates [`sqlx::Error`]. A `kind`, `priority` or `subject_kind` outside the
/// schema's constraints surfaces as a check-violation.
pub async fn enqueue(pool: &PgPool, job: &NewJob) -> Result<Option<i64>, sqlx::Error> {
    let sql = "INSERT INTO job (kind, priority, subject_kind, subject_id, payload, run_after) \
         VALUES ($1, $2, $3, $4, $5, COALESCE($6, now())) \
         ON CONFLICT (kind, subject_kind, subject_id) \
             WHERE status IN ('pending', 'running') \
         DO UPDATE SET priority = LEAST(job.priority, EXCLUDED.priority), updated_at = now() \
         RETURNING id, (xmax = 0) AS inserted";
    let row: (i64, bool) = sqlx::query_as(sql)
        .bind(&job.kind)
        .bind(job.priority)
        .bind(&job.subject_kind)
        .bind(&job.subject_id)
        .bind(&job.payload)
        .bind(job.run_after)
        .fetch_one(pool)
        .await?;
    Ok(if row.1 { Some(row.0) } else { None })
}

/// Claim up to `limit` due jobs for `worker_id`.
///
/// The inner `SELECT … FOR UPDATE SKIP LOCKED` is what makes this safe with several
/// workers: each one only ever sees rows nobody else has locked. `attempts` is
/// incremented on claim, not on failure, so a worker that dies mid-job still consumes
/// an attempt and a permanently crashing job eventually stops being retried.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn claim(pool: &PgPool, worker_id: &str, limit: i64) -> Result<Vec<Job>, sqlx::Error> {
    sqlx::query_as::<_, Job>(
        "WITH due AS ( \
             SELECT id FROM job \
             WHERE status = 'pending' AND run_after <= now() AND attempts < max_attempts \
             ORDER BY priority, run_after \
             LIMIT $2 \
             FOR UPDATE SKIP LOCKED \
         ) \
         UPDATE job SET status = 'running', locked_at = now(), locked_by = $1, \
             attempts = job.attempts + 1, updated_at = now() \
         FROM due WHERE job.id = due.id \
         RETURNING job.*",
    )
    .bind(worker_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Mark a job finished successfully.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn complete(pool: &PgPool, job_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE job SET status = 'done', finished_at = now(), locked_at = NULL, locked_by = NULL, \
             last_error = NULL, updated_at = now() \
         WHERE id = $1",
    )
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a failure: retry with backoff, or give up once attempts are exhausted.
///
/// The attempt count was already incremented at claim time, so this only decides
/// between `pending` (with a new `run_after`) and `failed`.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn fail(
    pool: &PgPool,
    job_id: i64,
    error: &str,
    retry_after: time::Duration,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE job SET \
             status = CASE WHEN attempts >= max_attempts THEN 'failed' ELSE 'pending' END, \
             run_after = CASE WHEN attempts >= max_attempts THEN run_after ELSE now() + $3 END, \
             finished_at = CASE WHEN attempts >= max_attempts THEN now() ELSE NULL END, \
             locked_at = NULL, locked_by = NULL, last_error = $2, updated_at = now() \
         WHERE id = $1",
    )
    .bind(job_id)
    .bind(error)
    .bind(retry_after)
    .execute(pool)
    .await?;
    Ok(())
}

/// Return jobs whose worker stopped reporting back to the queue.
///
/// A lease that outlives its holder means the process died. Those rows go back to
/// `pending` so another worker picks them up; their attempt was already counted, so a
/// job that reliably kills its worker still terminates.
///
/// Returns the number of jobs recovered.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn reap_expired_leases(pool: &PgPool, lease: time::Duration) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE job SET status = 'pending', locked_at = NULL, locked_by = NULL, updated_at = now(), \
             last_error = COALESCE(last_error, 'lease expired') \
         WHERE status = 'running' AND locked_at < now() - $1",
    )
    .bind(lease)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// How much work is outstanding, by priority, for a health metric.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn backlog_by_priority(pool: &PgPool) -> Result<Vec<(i16, i64)>, sqlx::Error> {
    sqlx::query_as::<_, (i16, i64)>(
        "SELECT priority, count(*) FROM job WHERE status = 'pending' GROUP BY priority \
         ORDER BY priority",
    )
    .fetch_all(pool)
    .await
}
