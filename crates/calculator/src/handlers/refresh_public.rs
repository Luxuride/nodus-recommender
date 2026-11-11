//! `refresh_public` — the instance's federated public page, written into the pool.
//!
//! Personal affinity can be empty on a quiet instance while the federated timeline
//! still has fresh posts. This handler reads a few pages of that timeline and stores
//! the statuses and their authors, so the post feed has current material to rank.
//! Reblogs are skipped: the feed wants authored posts, not a boosted copy.

use core::future::Future;
use core::pin::Pin;
use std::collections::HashSet;
use std::time::Duration;

use nodus_instance::InstanceApi;
use nodus_instance::model::{PageParams, StreamKind};
use nodus_store::models::{Job, NewAccount};
use nodus_store::{accounts, statuses, users};
use sqlx::PgPool;

use super::refresh_account::{RefreshConfig, fetch};
use super::{Handler, JobError};

/// How many federated pages one tick reads. Three is enough to cover a missed tick on
/// a quiet instance without turning every refresh into a crawl.
const PAGES: usize = 3;

/// The federated page handler.
#[derive(Debug)]
pub struct RefreshPublic {
    pool: PgPool,
    config: RefreshConfig,
}

impl RefreshPublic {
    /// A handler bound to one pool and one pacing config.
    #[must_use]
    pub fn new(pool: PgPool, config: RefreshConfig) -> Self {
        Self { pool, config }
    }
}

impl Handler for RefreshPublic {
    fn kind(&self) -> &'static str {
        "refresh_public"
    }

    fn run<'a>(
        &'a self,
        job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move {
            let Some(host) = job.subject_id.as_deref() else {
                return Err(JobError::permanent("refresh_public needs an instance host"));
            };
            let Some(pooled) = users::pick_token(&self.pool, host, &[])
                .await
                .map_err(|error| database(&error))?
            else {
                return Err(JobError::retry(
                    Duration::from_secs(300),
                    format!("no token is available for `{host}`; the public page waits"),
                ));
            };
            let api = fetch::transport(host, &pooled.access_token_enc, &self.config)?;
            let mut statuses = Vec::new();
            let mut params = PageParams::first(self.config.page_size);
            for _ in 0..PAGES {
                let page = api
                    .timeline(StreamKind::Federated, params)
                    .await
                    .map_err(classify)?;
                let next = page.next_max_id.clone();
                statuses.extend(page.items);
                let Some(next) = next else {
                    break;
                };
                params = PageParams::after(next, self.config.page_size);
            }

            let mut authors = Vec::new();
            let mut seen = HashSet::new();
            for status in &statuses {
                if status.reblog.is_some() {
                    continue;
                }
                let Some(account) = account_row(host, &status.account) else {
                    continue;
                };
                if seen.insert(account.mastodon_id) {
                    authors.push(account);
                }
            }
            accounts::upsert_many(&self.pool, &authors)
                .await
                .map_err(|error| database(&error))?;

            let mut batch = Vec::new();
            let mut avatars = HashSet::new();
            for status in &statuses {
                if status.reblog.is_some() {
                    continue;
                }
                let Ok(mastodon_id) = status.account.id.parse::<i64>() else {
                    continue;
                };
                let Some(local_id) = accounts::find_id(&self.pool, host, mastodon_id)
                    .await
                    .map_err(|error| database(&error))?
                else {
                    continue;
                };
                if avatars.insert(local_id) && !status.account.avatar.trim().is_empty() {
                    accounts::set_avatar(&self.pool, local_id, &status.account.avatar)
                        .await
                        .map_err(|error| database(&error))?;
                }
                if let Some(row) = fetch::map_status(host, local_id, status) {
                    batch.push(row);
                }
            }
            let written = statuses::upsert_many(&self.pool, &batch)
                .await
                .map_err(|error| database(&error))?;
            tracing::info!(%host, fetched = batch.len(), written, "refresh_public finished");
            Ok(())
        })
    }
}

fn account_row(host: &str, account: &nodus_instance::model::Account) -> Option<NewAccount> {
    let mastodon_id = account.id.parse::<i64>().ok()?;
    Some(NewAccount {
        instance_host: host.to_owned(),
        mastodon_id,
        acct: account.acct.clone(),
        display_name: Some(account.display_name.clone()),
        followers_count: i32::try_from(account.followers_count).unwrap_or(i32::MAX),
        bot: account.bot,
        locked: account.locked,
        discoverable: account.discoverable,
        indexable: account.indexable,
        noindex: account.noindex,
    })
}

fn database(error: &sqlx::Error) -> JobError {
    JobError::retry(Duration::from_secs(60), format!("database: {error}"))
}

fn classify(error: nodus_instance::Error) -> JobError {
    match error {
        nodus_instance::Error::RateLimited { retry_after_s } => JobError::retry(
            Duration::from_secs(u64::from(retry_after_s.max(1))),
            format!("instance rate limit: retry after {retry_after_s}s"),
        ),
        nodus_instance::Error::Unauthorized => {
            JobError::permanent("401 from the instance: token revoked or scope removed")
        }
        nodus_instance::Error::Transport(message) => {
            JobError::retry(Duration::from_secs(60), format!("transport: {message}"))
        }
        other => JobError::retry(
            Duration::from_secs(300),
            format!("public timeline: {other}"),
        ),
    }
}
