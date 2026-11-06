//! `sync_user` — P0, one user's graph snapshot.
//!
//! Reads the stored token, walks the user's following list, favourites and
//! bookmarks, and writes `account` + `follow_edge` rows. Roughly ten requests per
//! user per day, which is why it is the highest tier: without it nothing else has
//! a graph to expand.
//!
//! Deliberately not implemented yet, and the honest failure mode matters: returning
//! a *retry* rather than a success means the job stays visible in `last_error` and
//! in the backlog, instead of being recorded as done while no rows were written.
//! A fake success would make the M1 exit criterion — "a real token populates
//! `account` and `follow_edge`" — unverifiable.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use nodus_store::models::Job;
use tracing::{info, warn};

use super::{Handler, JobError};

/// How long to wait before trying a still-unimplemented tier again.
const NOT_YET: Duration = Duration::from_secs(15 * 60);

/// The P0 handler.
#[derive(Debug, Default)]
pub struct SyncUser;

impl SyncUser {
    /// A handler with the default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Handler for SyncUser {
    fn kind(&self) -> &'static str {
        "sync_user"
    }

    fn run<'a>(
        &'a self,
        job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(subject) = job.subject_id.as_deref() else {
                // A `sync_user` without a subject cannot be retried into working: the
                // enqueue path always sets one, so this is a bug or a hand-written row.
                return Err(JobError::permanent("sync_user has no subject_id"));
            };

            info!(
                job = job.id,
                user = subject,
                attempt = job.attempts,
                "sync_user claimed"
            );

            // Next commit: decrypt via `nodus_store::crypto::TokenSealer`, load the
            // user, and walk the instance with `nodus_instance::InstanceApi`. The
            // pieces below the transport (store, sealing, queue) already exist; the
            // request layer is the remaining gap, so the work is reported as pending
            // rather than silently skipped.
            warn!(
                job = job.id,
                user = subject,
                "sync_user has no transport yet"
            );
            Err(JobError::retry(
                NOT_YET,
                "instance transport not implemented",
            ))
        })
    }
}
