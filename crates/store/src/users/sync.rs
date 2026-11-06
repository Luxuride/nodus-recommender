//! Background sync on a user's own token: on, off, and read back for the calculator.

use sqlx::PgPool;
use uuid::Uuid;

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
