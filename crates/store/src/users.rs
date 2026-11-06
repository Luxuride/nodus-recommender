//! Reads and writes for [`AppUser`].
//!
//! Every function takes a pool rather than a connection: the feed path issues a
//! handful of independent reads and does not need them in one transaction, while the
//! sync path does its bookkeeping per statement. Anything that must be atomic is
//! written as a single statement, which is also why most of these are one query.

use crate::models::AppUser;
use sqlx::PgPool;
use uuid::Uuid;

/// Columns of [`AppUser`], with `citext` narrowed to `text` for sqlx.
const USER_COLUMNS: &str = "id, instance_host::text AS instance_host, mastodon_account_id, \
     acct::text AS acct, sync_enabled, telemetry_opt_in, created_at, last_seen_at, deleted_at";

/// Make sure the `instance` row exists, so account and user rows can reference it.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn ensure_instance(pool: &PgPool, host: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO instance (host, last_contact_at) VALUES ($1, now()) \
         ON CONFLICT (host) DO UPDATE SET last_contact_at = now()",
    )
    .bind(host)
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert the user if unknown, otherwise refresh `acct` and `last_seen_at`.
///
/// The unique key is `(instance_host, mastodon_account_id)`, not the handle: people
/// rename themselves, and a rename must not fork their recommendation history.
///
/// # Errors
///
/// Propagates [`sqlx::Error`]; a missing `instance` row is a foreign-key violation
/// and therefore a caller bug.
pub async fn upsert(
    pool: &PgPool,
    instance_host: &str,
    mastodon_account_id: i64,
    acct: &str,
) -> Result<AppUser, sqlx::Error> {
    let sql = format!(
        "INSERT INTO app_user (instance_host, mastodon_account_id, acct) VALUES ($1, $2, $3) \
         ON CONFLICT (instance_host, mastodon_account_id) \
         DO UPDATE SET acct = EXCLUDED.acct, last_seen_at = now() \
         RETURNING {USER_COLUMNS}"
    );
    sqlx::query_as::<_, AppUser>(&sql)
        .bind(instance_host)
        .bind(mastodon_account_id)
        .bind(acct)
        .fetch_one(pool)
        .await
}

/// Look a user up by the fingerprint of the bearer token presented on a request.
///
/// This is the hot path of every authenticated call, so it is a single indexed
/// comparison; the plaintext token is never stored, and the fingerprint is a prefix
/// of its SHA-256 so a leaked database cannot be replayed against an instance.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn find_by_token_fingerprint(
    pool: &PgPool,
    fingerprint: &[u8],
) -> Result<Option<AppUser>, sqlx::Error> {
    let sql = format!(
        "SELECT {USER_COLUMNS} FROM app_user \
         WHERE token_fingerprint = $1 AND deleted_at IS NULL"
    );
    sqlx::query_as::<_, AppUser>(&sql)
        .bind(fingerprint)
        .fetch_optional(pool)
        .await
}

/// Store an encrypted token and turn background sync on.
///
/// `encrypted` is the caller's AEAD output (`nonce ‖ ciphertext`); this crate never
/// sees a plaintext token and holds no key.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn set_sync_token(
    pool: &PgPool,
    user_id: Uuid,
    encrypted: &[u8],
    scopes: &[String],
    fingerprint: &[u8],
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE app_user SET access_token_enc = $2, token_scopes = $3, token_fingerprint = $4, \
             sync_enabled = true, last_seen_at = now() \
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(user_id)
    .bind(encrypted)
    .bind(scopes)
    .bind(fingerprint)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Forget the token and stop background sync.
///
/// The token itself is also revoked at the instance by the caller; here we only
/// ensure that nothing can be decrypted afterwards, even from a backup taken between
/// the two steps.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn clear_sync_token(pool: &PgPool, user_id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE app_user SET access_token_enc = NULL, token_scopes = NULL, \
             token_fingerprint = NULL, sync_enabled = false \
         WHERE id = $1",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Record a telemetry decision. Consent and sync are independent switches.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn set_telemetry_opt_in(
    pool: &PgPool,
    user_id: Uuid,
    opt_in: bool,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE app_user SET telemetry_opt_in = $2, \
             telemetry_opt_in_at = CASE WHEN $2 THEN now() ELSE NULL END \
         WHERE id = $1",
    )
    .bind(user_id)
    .bind(opt_in)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Soft delete: the row survives for the grace period, the data does not.
///
/// Derived rows (`user_similarity`, `account_affinity`, queued jobs) are removed by
/// the caller's deletion job in the same transaction it uses for the rest.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn soft_delete(pool: &PgPool, user_id: Uuid) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE app_user SET deleted_at = now(), access_token_enc = NULL, \
             token_scopes = NULL, token_fingerprint = NULL, sync_enabled = false \
         WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// What the calculator needs to sync one user, and nothing more.
///
/// Deliberately not a `SELECT *` of `app_user`: the sealed token is the only secret
/// in the schema, and a struct that only ever exists on the sync path is easier to
/// keep out of logs and API responses than a general-purpose row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SyncMaterial {
    /// The user this material belongs to.
    pub id: Uuid,
    /// Their instance, `citext` narrowed to `text`.
    pub instance_host: String,
    /// Their account id on that instance.
    pub mastodon_account_id: i64,
    /// `nonce ‖ ciphertext`, opened with `TOKEN_ENC_KEY`.
    pub access_token_enc: Vec<u8>,
    /// Scopes granted when the token was stored.
    pub token_scopes: Vec<String>,
}

impl SyncMaterial {
    /// Whether the token carries the scope a crawl needs.
    ///
    /// Checked before the first request rather than discovered as a 403 halfway
    /// through a follow list: a missing scope is a consent problem the user can fix
    /// in one click, not a transient failure to retry.
    #[must_use]
    pub fn can_read_graph(&self) -> bool {
        self.token_scopes
            .iter()
            .any(|scope| scope == "read:follows" || scope == "read" || scope == "follow")
    }
}

/// Load the sync material for a user, if background sync is on for them.
///
/// Returns `None` when the user turned sync off, deleted their account, or never
/// stored a token. Every one of those is a permanent answer, not a retry: the job
/// should stop rather than ask again in fifteen minutes.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn sync_material(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<SyncMaterial>, sqlx::Error> {
    sqlx::query_as::<_, SyncMaterial>(
        "SELECT id, instance_host::text AS instance_host, mastodon_account_id, \
             access_token_enc, token_scopes \
         FROM app_user \
         WHERE id = $1 AND sync_enabled AND deleted_at IS NULL \
             AND access_token_enc IS NOT NULL",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}
