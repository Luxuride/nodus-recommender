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

use core::fmt::{self, Write as _};
use core::future::Future;
use core::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use nodus_cache::{Budget, InMemoryBudget};
use nodus_instance::model::{LocalId, PageParams};
use nodus_instance::{Error as InstanceError, HttpInstance, Instance, InstanceApi, SyncApi};
use nodus_store::crypto::{TokenSealer, fingerprint as token_fingerprint};
use nodus_store::models::{Job, NewAccount};
use nodus_store::{accounts, jobs, users};
use sqlx::PgPool;
use tracing::{info, warn};
use uuid::Uuid;

use super::{Handler, JobError};

/// Burst for the in-process budget: one second's worth of requests can go at once,
/// which is what lets a first page of 80 accounts arrive without stalling.
const BURST: u32 = 5;

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
    /// The configured rate, from `SYNC_MAX_RPS_PER_HOST`.
    pub requests_per_second: f64,
    /// The per-host request budget, once one is shared (Redis); `None` means the
    /// handler builds the in-process one on construction.
    ///
    /// It lives here rather than in a new state struct because `SyncConfig` is
    /// already what `Registry::standard` hands to every handler, and this port is
    /// the only shared state a crawl needs beyond the pool. A struct wrapping
    /// `pool` + `config` would add a layer without changing what is shared.
    pub budget: Option<Arc<dyn Budget>>,
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
            requests_per_second: rps,
            budget: None,
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

    /// Use `budget` as the shared per-host request budget.
    ///
    /// Without this the handler falls back to [`InMemoryBudget`], which is correct
    /// for one replica and wrong for several: each replica would get its own bucket
    /// and the effective rate would be the configured rate times the replica count.
    #[must_use]
    pub fn with_budget(mut self, budget: Arc<dyn Budget>) -> Self {
        self.budget = Some(budget);
        self
    }

    /// The configured rate as whole requests per second, for the budget backends.
    ///
    /// Both implementations take `u32`, while the configured rate is a fraction
    /// (0.5 means one request every two seconds). The bucket is sized by the ceiling
    /// and the token's own spend pattern supplies the fractional pacing.
    /// Float-to-int casts saturate in Rust, so a huge rate lands at `u32::MAX`,
    /// `NaN` at 0, and the floor of one keeps the bucket usable.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[must_use]
    pub fn budget_rate(&self) -> u32 {
        (self.requests_per_second.ceil() as u32).max(1)
    }
}

/// The job's subject: whose graph this run rewrites.
#[derive(Debug, Clone)]
struct Subject {
    /// The `app_user` row, so a revocation can be recorded against the right person.
    id: Uuid,
    /// The instance whose tokens may be spent on this crawl.
    instance_host: String,
    /// The subject's remote id, which names the `following` collection.
    mastodon_account_id: i64,
}

/// One token that may be spent on the subject's graph.
struct Token {
    /// The `app_user` whose consent this token carries; cleared on a 401.
    owner: Uuid,
    /// Plaintext bearer token, held only for the length of a crawl.
    secret: String,
    /// SHA-256 of `secret`, from the store's `fingerprint`; names the budget bucket
    /// and the `app_user.token_fingerprint` row.
    fingerprint: [u8; 32],
}

impl Token {
    fn new(owner: Uuid, secret: String) -> Self {
        let fingerprint = token_fingerprint(&secret);
        Self {
            owner,
            secret,
            fingerprint,
        }
    }

    /// The token half of the D23 budget key, `budget:<instance_host>:<hex>`.
    ///
    /// Written out by hand rather than with a hex crate: 32 bytes is one `write!`
    /// loop, and the same bytes are also a Postgres `token_fingerprint`.
    fn bucket(&self) -> String {
        let mut hex = String::with_capacity(64);
        for byte in self.fingerprint {
            write!(hex, "{byte:02x}").ok();
        }
        hex
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never let the bearer token reach a log line through a `{:?}`.
        f.debug_struct("Token")
            .field("owner", &self.owner)
            .field("fingerprint", &self.bucket())
            .finish_non_exhaustive()
    }
}

/// Why a token was retired from this run, and how long the host asked us to wait.
#[derive(Debug)]
enum Retired {
    /// A 401: the token is dead and must be cleared.
    Revoked,
    /// A 429: the token is fine, the host is not, for `Retry-After` seconds.
    RateLimited(Duration),
}

/// A token-level failure, separated from a job-level one.
#[derive(Debug)]
enum AttemptError {
    /// Drop this token and pick the next one.
    Retired(Retired),
    /// Stop the job; every token would hit the same thing.
    Fatal(JobError),
}

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

    /// Opens the subject's stored token, whose failure is the person's to fix.
    fn open_own(&self, owner: Uuid, sealed: &[u8]) -> Result<Token, JobError> {
        let secret = self.config.sealer.open(sealed).map_err(|e| {
            JobError::permanent(format!("the stored token cannot be decrypted: {e}"))
        })?;
        Ok(Token::new(owner, secret))
    }

    /// Opens a pooled token. A failure here is retried rather than made permanent:
    /// one corrupt row must not stop every other token for the instance.
    fn open_pooled(&self, pooled: &users::PooledToken) -> Result<Token, JobError> {
        let secret = self
            .config
            .sealer
            .open(&pooled.access_token_enc)
            .map_err(|e| {
                JobError::retry(
                    Duration::from_secs(300),
                    format!("a pooled token cannot be decrypted: {e}"),
                )
            })?;
        Ok(Token::new(pooled.user_id, secret))
    }

    /// One walk of the subject's graph with one token.
    async fn crawl(
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

    /// Resolves the user's own `account` row, creating it from the token's identity
    /// when this is the first sync.
    ///
    /// The row has to exist before edges can point at it, and `verify_credentials`
    /// is the only source of a trustworthy `followers_count` for the user's own
    /// account — the alternative, inserting zeros from `app_user`, would quietly
    /// make the user look unpopular in every damping calculation.
    ///
    /// `verify_credentials` answers for whoever owns the token, so only the subject's
    /// own token may create the row; a pooled token would attach the subject's edges
    /// to the wrong account.
    async fn own_account(
        &self,
        api: &HttpInstance,
        token: &Token,
        subject: &Subject,
        is_own: bool,
    ) -> Result<i64, AttemptError> {
        let mastodon_id = subject.mastodon_account_id;
        if let Some(local) = accounts::find_id(&self.pool, &subject.instance_host, mastodon_id)
            .await
            .map_err(|e| AttemptError::Fatal(database(&e)))?
        {
            return Ok(local);
        }
        if !is_own {
            return Err(AttemptError::Fatal(JobError::permanent(
                "the subject's account row is missing and only its own token can create it; \
                 re-authorise",
            )));
        }

        self.charge(token, &subject.instance_host).await?;
        let account = api.verify_credentials().await.map_err(classify)?;
        accounts::upsert_many(
            &self.pool,
            &[NewAccount {
                instance_host: subject.instance_host.clone(),
                mastodon_id,
                acct: account.acct.clone(),
                display_name: optional_text(&account.display_name),
                followers_count: i32::try_from(account.followers_count).unwrap_or(i32::MAX),
                bot: account.bot,
                locked: account.locked,
            }],
        )
        .await
        .map_err(|e| AttemptError::Fatal(database(&e)))?;

        accounts::find_id(&self.pool, &subject.instance_host, mastodon_id)
            .await
            .map_err(|e| AttemptError::Fatal(database(&e)))?
            .ok_or_else(|| {
                AttemptError::Fatal(JobError::retry(
                    Duration::from_secs(60),
                    "own account did not persist",
                ))
            })
    }

    /// Walks the following list, upserting accounts and collecting local ids.
    async fn collect_follows(
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
    async fn charge(&self, token: &Token, host: &str) -> Result<(), AttemptError> {
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
                    let edges =
                        accounts::replace_follows(&self.pool, own_id, &follows, "server_sync")
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

                    // Similarity and affinity are a whole-graph question; hand it to
                    // the tier that is allowed to take the recompute lock.
                    jobs::enqueue(
                        &self.pool,
                        &jobs::NewJob::pending(
                            "recompute_affinity",
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

/// The failure to report when the token pool is empty.
///
/// A revoked subject token is the person's to fix, so it stays permanent even if
/// every other token was merely rate-limited; otherwise the job is retried with the
/// shortest delay the instance asked for.
fn exhausted(host: &str, revoked_subject: bool, rate_limited: Option<Duration>) -> JobError {
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
fn classify(error: InstanceError) -> AttemptError {
    match error {
        InstanceError::RateLimited { retry_after_s } => AttemptError::Retired(
            Retired::RateLimited(Duration::from_secs(u64::from(retry_after_s.max(1)))),
        ),
        InstanceError::Unauthorized => AttemptError::Retired(Retired::Revoked),
        other => AttemptError::Fatal(transport(other)),
    }
}

/// An instance sends `""` for "no display name"; the column stores `NULL`.
fn optional_text(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
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
    use super::{AttemptError, Retired, Token, classify, transport};
    use crate::handlers::JobError;
    use nodus_instance::Error as InstanceError;
    use uuid::Uuid;

    #[test]
    fn a_401_retires_the_token_instead_of_failing_the_job() {
        assert!(matches!(
            classify(InstanceError::Unauthorized),
            AttemptError::Retired(Retired::Revoked)
        ));
    }

    #[test]
    fn a_rate_limit_retires_the_token_with_the_instances_delay() {
        match classify(InstanceError::RateLimited { retry_after_s: 42 }) {
            AttemptError::Retired(Retired::RateLimited(delay)) => {
                assert_eq!(delay.as_secs(), 42);
            }
            other => panic!("expected a rate-limited token, got {other:?}"),
        }
    }

    #[test]
    fn a_transport_failure_is_retried() {
        assert!(matches!(
            transport(InstanceError::Transport("timeout".to_owned())),
            JobError::Retry { .. }
        ));
    }

    #[test]
    fn the_budget_bucket_is_the_lowercase_fingerprint_hex() {
        let token = Token::new(Uuid::nil(), "secret".to_owned());
        let bucket = token.bucket();
        assert_eq!(bucket.len(), 64);
        assert!(
            bucket
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }
}
