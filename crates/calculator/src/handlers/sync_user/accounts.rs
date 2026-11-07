//! The subject's own account row and the tokens a crawl may spend.
//!
//! [`Subject`] is the local `accounts` row the job resolved; [`Token`] is one
//! sealed token opened for use, with the budget bucket key derived from it.
//! `open_own`/`open_pooled` decide which row a crawl starts from, [`own_account`]
//! resolves the subject's own row (creating it when the follow list is empty),
//! and [`optional_text`] is the small helper the follow rows share.

use core::fmt::{self, Write as _};
use std::time::Duration;

use nodus_instance::InstanceApi;
use nodus_instance::HttpInstance;
use nodus_store::accounts;
use nodus_store::crypto::fingerprint as token_fingerprint;
use nodus_store::models::NewAccount;
use nodus_store::users;
use uuid::Uuid;

use super::follows::{AttemptError, classify};
use super::{JobError, SyncUser, database};

/// The job's subject: whose graph this run rewrites.
#[derive(Debug, Clone)]
pub(super) struct Subject {
    /// The `app_user` row, so a revocation can be recorded against the right person.
    pub(super) id: Uuid,
    /// The instance whose tokens may be spent on this crawl.
    pub(super) instance_host: String,
    /// The subject's remote id, which names the `following` collection.
    pub(super) mastodon_account_id: i64,
}

/// One token that may be spent on the subject's graph.
pub(super) struct Token {
    /// The `app_user` whose consent this token carries; cleared on a 401.
    pub(super) owner: Uuid,
    /// Plaintext bearer token, held only for the length of a crawl.
    pub(super) secret: String,
    /// SHA-256 of `secret`, from the store's `fingerprint`; names the budget bucket
    /// and the `app_user.token_fingerprint` row.
    pub(super) fingerprint: [u8; 32],
}

impl Token {
    pub(super) fn new(owner: Uuid, secret: String) -> Self {
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
    pub(super) fn bucket(&self) -> String {
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

impl SyncUser {
    /// Opens the subject's stored token, whose failure is the person's to fix.
    pub(super) fn open_own(&self, owner: Uuid, sealed: &[u8]) -> Result<Token, JobError> {
        let secret = self.config.sealer.open(sealed).map_err(|e| {
            JobError::permanent(format!("the stored token cannot be decrypted: {e}"))
        })?;
        Ok(Token::new(owner, secret))
    }

    /// Opens a pooled token. A failure here is retried rather than made permanent:
    /// one corrupt row must not stop every other token for the instance.
    pub(super) fn open_pooled(&self, pooled: &users::PooledToken) -> Result<Token, JobError> {
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
    pub(super) async fn own_account(
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
}

/// An instance sends `""` for "no display name"; the column stores `NULL`.
pub(super) fn optional_text(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_owned())
}
