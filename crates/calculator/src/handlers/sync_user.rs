//! `sync_user` — P0, one user's follow graph.
//!
//! Reads the stored token, walks the user's following list, and writes `account` and
//! `follow_edge` rows. Roughly ten requests per user per day, which is why it is the
//! highest tier: without it nothing else has a graph to expand.
//!
//! Three properties are deliberate:
//!
//! * **A revocation is permanent.** A 401 means the user's token is gone; retrying
//!   would hammer the instance with a token that will never work again, so the job
//!   stops and the stored copy is cleared in the same breath.
//! * **A rate limit is not a failure.** The instance's `Retry-After` is honoured as
//!   the retry delay, so the queue paces itself against the instance's own answer
//!   instead of a guess of ours.
//! * **Edges are replaced, not merged.** The following list we just read *is* the
//!   graph; keeping an unfollowed account alive would make similarity depend on what
//!   used to be true.

use core::future::Future;
use core::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use nodus_instance::model::{LocalId, PageParams};
use nodus_instance::{Error as InstanceError, HttpInstance, Instance, InstanceApi, SyncApi};
use nodus_store::crypto::TokenSealer;
use nodus_store::models::{Job, NewAccount};
use nodus_store::{accounts, jobs, users};
use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

use super::{Handler, JobError};

/// How a crawl behaves: request spacing, page size and a hard page cap.
///
/// The cap is not paranoia — one account following 20 000 people would otherwise
/// spend the whole instance budget on a single user. When it trips, the graph is
/// truncated and logged, and the next run starts from the top of the list again.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// The sealing key, shared with `nodus-feeds`.
    pub sealer: Arc<TokenSealer>,
    /// Accounts per request; Mastodon caps this at 80.
    pub page_size: u16,
    /// Hard cap on pages per run.
    pub max_pages: usize,
    /// Delay between requests, from `SYNC_MAX_RPS_PER_HOST`.
    pub request_interval: Duration,
    /// Scheme used to reach an instance: `https`, except for a development instance
    /// that serves plain HTTP on localhost.
    pub scheme: String,
}

impl SyncConfig {
    /// A config with Mastodon's documented page size and our default pacing.
    #[must_use]
    pub fn new(sealer: Arc<TokenSealer>, max_rps: f64) -> Self {
        let rps = if max_rps > 0.0 { max_rps } else { 1.0 };
        Self {
            sealer,
            page_size: 80,
            max_pages: 250,
            request_interval: Duration::from_secs_f64(1.0 / rps),
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
    /// Only a development instance serving plain HTTP needs this; federation in
    /// production is HTTPS-only, and a scheme that could be set per user would be a
    /// way to point our token at an attacker's server.
    #[must_use]
    pub fn with_scheme(mut self, scheme: impl Into<String>) -> Self {
        self.scheme = scheme.into();
        self
    }
}

/// The P0 handler.
#[derive(Debug)]
pub struct SyncUser {
    pool: PgPool,
    config: SyncConfig,
}

impl SyncUser {
    /// A handler bound to one pool and one pacing config.
    #[must_use]
    pub fn new(pool: PgPool, config: SyncConfig) -> Self {
        Self { pool, config }
    }

    /// Resolves the user's own `account` row, creating it from the token's identity
    /// when this is the first sync.
    ///
    /// The row has to exist before edges can point at it, and `verify_credentials`
    /// is the only source of a trustworthy `followers_count` for the user's own
    /// account — the alternative, inserting zeros from `app_user`, would quietly
    /// make the user look unpopular in every damping calculation.
    async fn own_account(
        &self,
        api: &HttpInstance,
        material: &users::SyncMaterial,
    ) -> Result<i64, JobError> {
        let mastodon_id = material.mastodon_account_id;
        if let Some(local) = accounts::find_id(&self.pool, &material.instance_host, mastodon_id)
            .await
            .map_err(|e| database(&e))?
        {
            return Ok(local);
        }

        let account = api.verify_credentials().await.map_err(transport)?;
        accounts::upsert_many(
            &self.pool,
            &[NewAccount {
                instance_host: material.instance_host.clone(),
                mastodon_id,
                acct: account.acct.clone(),
                display_name: optional_text(&account.display_name),
                followers_count: i32::try_from(account.followers_count).unwrap_or(i32::MAX),
                bot: account.bot,
                locked: account.locked,
            }],
        )
        .await
        .map_err(|e| database(&e))?;

        accounts::find_id(&self.pool, &material.instance_host, mastodon_id)
            .await
            .map_err(|e| database(&e))?
            .ok_or_else(|| JobError::retry(Duration::from_secs(60), "own account did not persist"))
    }

    /// Walks the following list, upserting accounts and collecting local ids.
    async fn collect_follows(
        &self,
        api: &HttpInstance,
        material: &users::SyncMaterial,
    ) -> Result<Vec<i64>, JobError> {
        let me = LocalId(material.mastodon_account_id.to_string());
        let mut params = PageParams::first(self.config.page_size);
        let mut follows = Vec::new();
        let mut pages = 0_usize;

        loop {
            let page = api
                .following(&me, params.clone())
                .await
                .map_err(transport)?;
            if page.items.is_empty() {
                break;
            }

            let batch: Vec<NewAccount> = page
                .items
                .iter()
                .map(|account| {
                    Ok(NewAccount {
                        instance_host: material.instance_host.clone(),
                        mastodon_id: account.id.parse::<i64>().map_err(|e| {
                            JobError::permanent(format!(
                                "instance sent a non-numeric account id `{}`: {e}",
                                account.id
                            ))
                        })?,
                        acct: account.acct.clone(),
                        display_name: optional_text(&account.display_name),
                        followers_count: i32::try_from(account.followers_count).unwrap_or(i32::MAX),
                        bot: account.bot,
                        locked: account.locked,
                    })
                })
                .collect::<Result<_, JobError>>()?;

            accounts::upsert_many(&self.pool, &batch)
                .await
                .map_err(|e| database(&e))?;

            let mastodon_ids: Vec<i64> = batch.iter().map(|account| account.mastodon_id).collect();
            let resolved =
                accounts::resolve_ids(&self.pool, &material.instance_host, &mastodon_ids)
                    .await
                    .map_err(|e| database(&e))?;
            if resolved.len() != mastodon_ids.len() {
                warn!(
                    expected = mastodon_ids.len(),
                    resolved = resolved.len(),
                    "a follow page partially resolved after upsert"
                );
            }
            follows.extend(resolved.into_iter().map(|(local, _)| local));

            pages += 1;
            let Some(next) = page.next_max_id else {
                break;
            };
            if pages >= self.config.max_pages {
                warn!(
                    user = %material.id,
                    pages,
                    "follow list truncated at the page cap; the next run starts again"
                );
                break;
            }
            params = PageParams::after(next, self.config.page_size);
            // Spend the instance's budget between requests, not after the fact.
            tokio::time::sleep(self.config.request_interval).await;
        }

        follows.sort_unstable();
        follows.dedup();
        Ok(follows)
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
            let user_id = subject.parse::<Uuid>().map_err(|e| {
                JobError::permanent(format!("subject_id `{subject}` is not a uuid: {e}"))
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

            let token = self
                .config
                .sealer
                .open(&material.access_token_enc)
                .map_err(|e| {
                    JobError::permanent(format!("the stored token cannot be decrypted: {e}"))
                })?;
            let authority = format!("{}://{}", self.config.scheme, material.instance_host);
            let instance = Instance::parse(&authority).map_err(|e| {
                JobError::permanent(format!(
                    "`{}` is not a usable instance: {e}",
                    material.instance_host
                ))
            })?;
            let api = HttpInstance::new(&instance, token);

            let own_id = self.own_account(&api, &material).await?;
            let follows = match self.collect_follows(&api, &material).await {
                Ok(follows) => follows,
                Err(error) => {
                    // A revoked token is worth recording here rather than waiting for
                    // the next job: it stops the queue from retrying what a person
                    // must fix, and it means the encrypted copy does not outlive the
                    // permission it was granted for.
                    if is_revocation(&error) {
                        warn!(user = %material.id, "clearing the stored token after a 401");
                        let _ = users::clear_sync_token(&self.pool, material.id).await;
                    }
                    return Err(error);
                }
            };

            let edges = accounts::replace_follows(&self.pool, own_id, &follows, "server_sync")
                .await
                .map_err(|e| database(&e))?;

            info!(
                job = job.id,
                user = %material.id,
                accounts = follows.len(),
                edges,
                "sync_user finished"
            );

            // Similarity and affinity are a whole-graph question; hand it to the tier
            // that is allowed to take the recompute lock.
            jobs::enqueue(
                &self.pool,
                &jobs::NewJob::pending("recompute_affinity", 3, "user", Some(subject.to_owned())),
            )
            .await
            .map_err(|e| database(&e))?;

            Ok(())
        })
    }
}

/// An instance sends `""` for "no display name"; the column stores `NULL`.
fn optional_text(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
}

/// Whether a failure means the token is gone.
fn is_revocation(error: &JobError) -> bool {
    matches!(error, JobError::Permanent(_)) && error.message().contains("401")
}

/// Maps the transport's errors onto the queue's retry vocabulary.
fn transport(error: InstanceError) -> JobError {
    match error {
        InstanceError::RateLimited { retry_after_s } => JobError::retry(
            Duration::from_secs(u64::from(retry_after_s.max(1))),
            format!("instance rate limit: retry after {retry_after_s}s"),
        ),
        InstanceError::Unauthorized => {
            JobError::permanent("401 from the instance: token revoked or scope removed")
        }
        InstanceError::Transport(message) => {
            JobError::retry(Duration::from_secs(60), format!("transport: {message}"))
        }
        InstanceError::Status { status, message } => JobError::retry(
            Duration::from_secs(300),
            format!("instance answered {status}: {message}"),
        ),
        InstanceError::Decode(message) => JobError::retry(
            Duration::from_secs(300),
            format!("unexpected response shape: {message}"),
        ),
    }
}

/// Maps a database failure onto a retry: the instance side succeeded, so throwing the
/// work away would mean re-crawling it later.
fn database(error: &sqlx::Error) -> JobError {
    JobError::retry(Duration::from_secs(30), format!("database: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{is_revocation, transport};
    use crate::handlers::JobError;
    use nodus_instance::Error as InstanceError;

    #[test]
    fn a_401_becomes_a_permanent_failure() {
        let error = transport(InstanceError::Unauthorized);
        assert!(matches!(error, JobError::Permanent(_)));
        assert!(is_revocation(&error));
    }

    #[test]
    fn a_rate_limit_becomes_the_instances_delay() {
        match transport(InstanceError::RateLimited { retry_after_s: 42 }) {
            JobError::Retry { after, .. } => assert_eq!(after.as_secs(), 42),
            other @ JobError::Permanent(_) => panic!("expected a retry, got {other:?}"),
        }
    }

    #[test]
    fn a_transport_failure_is_retried() {
        assert!(matches!(
            transport(InstanceError::Transport("timeout".to_owned())),
            JobError::Retry { .. }
        ));
    }
}
