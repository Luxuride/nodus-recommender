//! Walking the following list, and turning instance errors into token-level ones.
//!
//! [`Retired`] and [`AttemptError`] are the vocabulary the token loop in
//! [`super`] matches on: a retired token stops this run, a fatal error stops the
//! job. [`crawl`] is one page walk, [`collect_follows`] resolves and upserts what
//! it found, [`charge`] spends one request of budget, and [`classify`] /
//! [`transport`] map what the instance reported.

use std::time::Duration;

use nodus_instance::HttpInstance;
use nodus_instance::model::{LocalId, PageParams};
use nodus_instance::{Error as InstanceError, Instance, SyncApi};
use nodus_store::accounts;
use nodus_store::models::NewAccount;
use tracing::warn;

use super::accounts::{Subject, Token, optional_text};
use super::{JobError, SyncUser, database};

/// Why a token was retired from this run, and how long the host asked us to wait.
#[derive(Debug)]
pub(super) enum Retired {
    /// A 401: the token is dead and must be cleared.
    Revoked,
    /// A 429: the token is fine, the host is not, for `Retry-After` seconds.
    RateLimited(Duration),
}

/// A token-level failure, separated from a job-level one.
#[derive(Debug)]
pub(super) enum AttemptError {
    /// Drop this token and pick the next one.
    Retired(Retired),
    /// Stop the job; every token would hit the same thing.
    Fatal(JobError),
}

impl SyncUser {
    /// One walk of the subject's graph with one token.
    pub(super) async fn crawl(
        &self,
        token: &Token,
        subject: &Subject,
        is_own: bool,
    ) -> Result<(i64, Vec<i64>), AttemptError> {
        let authority = format!("{}://{}", self.config.scheme, subject.instance_host);
        let instance = Instance::parse(&authority).map_err(|e| {
            AttemptError::Fatal(JobError::permanent(format!(
                "`{}` is not a usable instance: {e}",
                subject.instance_host
            )))
        })?;
        let api = HttpInstance::new(&instance, token.secret.clone());
        let own_id = self.own_account(&api, token, subject, is_own).await?;
        let follows = self.collect_follows(&api, token, subject).await?;
        Ok((own_id, follows))
    }

    /// Walks the following list, upserting accounts and collecting local ids.
    pub(super) async fn collect_follows(
        &self,
        api: &HttpInstance,
        token: &Token,
        subject: &Subject,
    ) -> Result<Vec<i64>, AttemptError> {
        let me = LocalId(subject.mastodon_account_id.to_string());
        let mut params = PageParams::first(self.config.page_size);
        let mut follows = Vec::new();
        let mut pages = 0_usize;

        loop {
            self.charge(token, &subject.instance_host).await?;
            let page = api.following(&me, params.clone()).await.map_err(classify)?;
            if page.items.is_empty() {
                break;
            }

            let batch: Vec<NewAccount> = page
                .items
                .iter()
                .map(|account| {
                    Ok(NewAccount {
                        instance_host: subject.instance_host.clone(),
                        mastodon_id: account.id.parse::<i64>().map_err(|e| {
                            AttemptError::Fatal(JobError::permanent(format!(
                                "instance sent a non-numeric account id `{}`: {e}",
                                account.id
                            )))
                        })?,
                        acct: account.acct.clone(),
                        display_name: optional_text(&account.display_name),
                        followers_count: i32::try_from(account.followers_count).unwrap_or(i32::MAX),
                        bot: account.bot,
                        locked: account.locked,
                        discoverable: account.discoverable,
                        indexable: account.indexable,
                        noindex: account.noindex,
                    })
                })
                .collect::<Result<_, AttemptError>>()?;

            accounts::upsert_many(&self.pool, &batch)
                .await
                .map_err(|e| AttemptError::Fatal(database(&e)))?;

            let mastodon_ids: Vec<i64> = batch.iter().map(|account| account.mastodon_id).collect();
            let resolved = accounts::resolve_ids(&self.pool, &subject.instance_host, &mastodon_ids)
                .await
                .map_err(|e| AttemptError::Fatal(database(&e)))?;
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
                    user = %subject.id,
                    pages,
                    "follow list truncated at the page cap; the next run starts again"
                );
                break;
            }
            params = PageParams::after(next, self.config.page_size);
            // The budget charges the next request at the top of the loop; no fixed
            // sleep here, because the token's remaining allowance decides the pace.
        }

        follows.sort_unstable();
        follows.dedup();
        Ok(follows)
    }

    /// Takes one request from the budget or hands the job back with the budget's
    /// own delay.
    ///
    /// It is deliberately not a sleep: a spent budget means this host has no
    /// allowance right now, and holding the worker (and the job's lease) while the
    /// instance refills spends a slot the queue could use elsewhere. A retry restarts
    /// the page walk, which is acceptable here because P0 is sized at roughly ten
    /// requests per user per day and the page cap bounds the rest.
    pub(super) async fn charge(&self, token: &Token, host: &str) -> Result<(), AttemptError> {
        let bucket = token.bucket();
        match self.budget.acquire(host, &bucket).await {
            Ok(true) => Ok(()),
            Ok(false) => {
                let delay = self
                    .budget
                    .retry_hint(host, &bucket)
                    .max(Duration::from_millis(100));
                Err(AttemptError::Fatal(JobError::retry(
                    delay,
                    format!("request budget for {host} is spent; rescheduling"),
                )))
            }
            Err(error) => {
                // A cache outage must not become a crawl outage: refusing is the
                // safe answer, and the hint keeps the retry from spinning.
                let delay = self
                    .budget
                    .retry_hint(host, &bucket)
                    .max(Duration::from_secs(1));
                warn!(%error, host, "request budget unavailable");
                Err(AttemptError::Fatal(JobError::retry(
                    delay,
                    format!("request budget unavailable: {error}"),
                )))
            }
        }
    }
}

/// The failure to report when the token pool is empty.
///
/// A revoked subject token is the person's to fix, so it stays permanent even if
/// every other token was merely rate-limited; otherwise the job is retried with the
/// shortest delay the instance asked for.
pub(super) fn exhausted(
    host: &str,
    revoked_subject: bool,
    rate_limited: Option<Duration>,
) -> JobError {
    if revoked_subject {
        return JobError::permanent(format!(
            "the user's token was revoked and no other token for {host} can continue"
        ));
    }
    if let Some(delay) = rate_limited {
        return JobError::retry(
            delay.max(Duration::from_secs(1)),
            format!("every token for {host} is rate-limited"),
        );
    }
    JobError::retry(
        Duration::from_secs(300),
        format!("no usable token for {host}"),
    )
}

/// Turns a token-level failure into a token switch or a job failure.
pub(super) fn classify(error: InstanceError) -> AttemptError {
    match error {
        InstanceError::RateLimited { retry_after_s } => AttemptError::Retired(
            Retired::RateLimited(Duration::from_secs(u64::from(retry_after_s.max(1)))),
        ),
        InstanceError::Unauthorized => AttemptError::Retired(Retired::Revoked),
        other => AttemptError::Fatal(transport(other)),
    }
}

/// Maps the transport's errors onto the queue's retry vocabulary.
pub(super) fn transport(error: InstanceError) -> JobError {
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
