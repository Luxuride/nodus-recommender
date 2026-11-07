//! `sync_user` — P0, one user's follow graph.
//!
//! Reads a stored token, walks the user's following list, and writes `account` and
//! `follow_edge` rows. Roughly ten requests per user per day, which is why it is the
//! highest tier: without it nothing else has a graph to expand.
//!
//! Four properties are deliberate:
//!
//! * **The subject's own token is tried first.** The job's `subject_id` names the
//!   user whose edges changed, and that user's consent is what makes the crawl
//!   legitimate.
//! * **A token is a pool, not an identity.** When the subject's token is rate-limited
//!   or revoked, another healthy token for the same instance may continue the walk
//!   (D23); only the subject's `follow_edge` rows are ever rewritten, and the pooled
//!   token's owner is never crawled.
//! * **A revocation is permanent for that token.** A 401 clears the dead token and
//!   moves to the next one; the job only fails permanently once the subject's own
//!   token was revoked and nothing else can continue.
//! * **Edges are replaced, not merged.** The following list we just read *is* the
//!   graph; keeping an unfollowed account alive would make similarity depend on what
//!   used to be true.
//!
//! What this handler deliberately does **not** do is crawl the accounts it discovers.
//! Syncing the graph of another account is a different job: `expand_follows` (P1)
//! owns the shared per-account crawl, and `refresh_account` (P2) owns refreshing a
//! known account. Turning `sync_user` into that crawler would make one user's login
//! fan out into arbitrary work and blur the tier boundaries the queue relies on.
//!
//! The directory divides along the concerns of one crawl: [`config`] holds the
//! pacing and the burst, [`accounts`] resolves the job's subject and opens the
//! tokens it may spend, [`follows`] walks the following list and classifies what
//! the instance answers, and this file keeps [`SyncUser`] with the token loop that
//! decides which of the others runs next.

use core::future::Future;
use core::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use nodus_cache::{Budget, InMemoryBudget};
use nodus_store::accounts as store_accounts;
use nodus_store::models::Job;
use nodus_store::{jobs, users};
use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

use super::{Handler, JobError};

mod accounts;
mod config;
mod follows;

pub use config::SyncConfig;

use accounts::Subject;
use config::BURST;
use follows::{AttemptError, Retired, exhausted};

/// The P0 handler.
#[derive(Debug)]
pub struct SyncUser {
    pool: PgPool,
    config: SyncConfig,
    /// Resolved once so the in-process fallback's buckets are shared by every job
    /// this replica runs; a fresh budget per job would make the rate meaningless.
    budget: Arc<dyn Budget>,
}

impl SyncUser {
    /// A handler bound to one pool and one pacing config.
    #[must_use]
    pub fn new(pool: PgPool, config: SyncConfig) -> Self {
        let budget = if let Some(budget) = config.budget.clone() {
            budget
        } else {
            warn!(
                "no shared request budget configured; per-host budgets are per replica \
                 (set REDIS_URL to share them)"
            );
            Arc::new(InMemoryBudget::new(config.budget_rate(), BURST))
        };
        Self {
            pool,
            config,
            budget,
        }
    }

    /// The token loop: the subject's own token first, then the host's pool.
    ///
    /// Each failed attempt either retires a token (clearing it when the instance said
    /// it was dead) or stops the job. The loop terminates because every retirement
    /// adds a fingerprint to `excluded`, and `pick_token` never returns an excluded
    /// row.
    async fn walk(
        &self,
        job: &Job,
        user_id: Uuid,
        subject: Subject,
        sealed: Vec<u8>,
    ) -> Result<(), JobError> {
        let mut stored = Some(sealed);
        let mut excluded: Vec<[u8; 32]> = Vec::new();
        let mut revoked_subject = false;
        let mut rate_limited: Option<Duration> = None;

        loop {
            let (token, is_own) = if let Some(sealed) = stored.take() {
                (self.open_own(subject.id, &sealed)?, true)
            } else {
                let Some(pooled) = users::pick_token(&self.pool, &subject.instance_host, &excluded)
                    .await
                    .map_err(|e| database(&e))?
                else {
                    return Err(exhausted(
                        &subject.instance_host,
                        revoked_subject,
                        rate_limited,
                    ));
                };
                (self.open_pooled(&pooled)?, false)
            };

            match self.crawl(&token, &subject, is_own).await {
                Ok((own_id, follows)) => {
                    let edges = store_accounts::replace_follows(
                        &self.pool,
                        own_id,
                        &follows,
                        "server_sync",
                    )
                    .await
                    .map_err(|e| database(&e))?;

                    info!(
                        job = job.id,
                        user = %subject.id,
                        token_owner = %token.owner,
                        pooled = !is_own,
                        accounts = follows.len(),
                        edges,
                        "sync_user finished"
                    );

                    // Similarity and affinity are a whole-graph question, and the same
                    // pass writes both (`account_affinity` is where the neighbours'
                    // follows land), so this enqueues the tier that owns the recompute
                    // lock rather than a second kind nobody handles.
                    jobs::enqueue(
                        &self.pool,
                        &jobs::NewJob::pending(
                            "recompute_similarity",
                            3,
                            "user",
                            Some(user_id.to_string()),
                        ),
                    )
                    .await
                    .map_err(|e| database(&e))?;

                    return Ok(());
                }
                Err(AttemptError::Fatal(error)) => return Err(error),
                Err(AttemptError::Retired(Retired::Revoked)) => {
                    // A dead token is worth recording here rather than waiting for
                    // the next job: it stops the queue from retrying what a person
                    // must fix, and the encrypted copy does not outlive the
                    // permission it was granted for.
                    warn!(
                        user = %subject.id,
                        token_owner = %token.owner,
                        "clearing a revoked token"
                    );
                    if let Err(error) = users::clear_sync_token(&self.pool, token.owner).await {
                        warn!(%error, "could not clear the revoked token");
                    }
                    if is_own {
                        revoked_subject = true;
                    }
                    excluded.push(token.fingerprint);
                }
                Err(AttemptError::Retired(Retired::RateLimited(delay))) => {
                    // The instance's own answer becomes state every replica reads;
                    // otherwise the next job walks into the same 429.
                    if let Err(error) = self.budget.hold(&subject.instance_host, delay).await {
                        warn!(
                            %error,
                            host = %subject.instance_host,
                            "could not record the host cooldown"
                        );
                    }
                    rate_limited = Some(rate_limited.map_or(delay, |current| current.min(delay)));
                    excluded.push(token.fingerprint);
                }
            }
        }
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
            let Some(raw) = job.subject_id.as_deref() else {
                // A `sync_user` without a subject cannot be retried into working: the
                // enqueue path always sets one, so this is a bug or a hand-written row.
                return Err(JobError::permanent("sync_user has no subject_id"));
            };
            let user_id = raw.parse::<Uuid>().map_err(|e| {
                JobError::permanent(format!("subject_id `{raw}` is not a uuid: {e}"))
            })?;

            let Some(material) = users::sync_material(&self.pool, user_id)
                .await
                .map_err(|e| database(&e))?
            else {
                return Err(JobError::permanent(
                    "background sync is off for this user (or the account is deleted)",
                ));
            };
            if !material.can_read_graph() {
                return Err(JobError::permanent(
                    "the stored token lacks read:follows; re-authorise to grant it",
                ));
            }

            let subject = Subject {
                id: material.id,
                instance_host: material.instance_host.clone(),
                mastodon_account_id: material.mastodon_account_id,
            };
            self.walk(job, user_id, subject, material.access_token_enc)
                .await
        })
    }
}

/// Maps a database failure onto a retry: the instance side succeeded, so throwing the
/// work away would mean re-crawling it later.
fn database(error: &sqlx::Error) -> JobError {
    JobError::retry(Duration::from_secs(30), format!("database: {error}"))
}

#[cfg(test)]
mod tests;
