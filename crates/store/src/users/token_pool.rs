//! The token pool: which token makes the next request for an instance.

use sqlx::PgPool;
use uuid::Uuid;

/// One token the pool may spend, with the account it belongs to.
///
/// D23 treats a token as a resource the worker schedules, not as a property of the
/// job: the instance, not the user, is the unit of work. Which user consented still
/// matters — it decides whose `app_user` row a 401 clears and whose budget the
/// request is charged to — so the owner id travels with the sealed copy.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PooledToken {
    /// The user whose consent (and token) this is.
    pub user_id: Uuid,
    /// `nonce ‖ ciphertext`, opened with `TOKEN_ENC_KEY`.
    pub access_token_enc: Vec<u8>,
    /// Scopes granted when the token was stored.
    pub token_scopes: Vec<String>,
    /// The owner's account id on that instance.
    pub mastodon_account_id: i64,
}

/// Claim the token for `instance_host` that should make the next request.
///
/// The order is D23's: the token with the most remaining budget, then the one used
/// longest ago. Remaining budget is not stored separately — every token on a host
/// refills at the same rate — so `token_last_used_at` is the clock that makes the
/// rotation fair: a token that has never been spent comes first (`NULLS FIRST`),
/// then the oldest stamp.
///
/// The claim and the stamp are one statement, and the subquery locks its row with
/// `FOR UPDATE SKIP LOCKED`. That is what keeps two workers from picking the same
/// token: the second transaction skips the locked row and takes the next one
/// instead of queueing behind it, and the winner's `token_last_used_at = now()` is
/// committed before the caller receives the token. Sequential calls therefore
/// return different tokens while an unspent one exists, which is the property the
/// pool tests pin.
///
/// Only tokens that can read a graph are poolable: `sync_enabled`, not deleted, a
/// sealed copy, and a scope in `read:follows`/`read`/`follow`. A row without a
/// fingerprint is also skipped, because the caller builds both the exclusion list
/// and the budget key from it. Fingerprints in `excluded_fingerprints` are skipped
/// so one token that answered 401 or 429 does not stop the run.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn pick_token(
    pool: &PgPool,
    instance_host: &str,
    excluded_fingerprints: &[[u8; 32]],
) -> Result<Option<PooledToken>, sqlx::Error> {
    let excluded: Vec<&[u8]> = excluded_fingerprints
        .iter()
        .map(<[u8; 32]>::as_slice)
        .collect();
    sqlx::query_as::<_, PooledToken>(
        "UPDATE app_user AS picked \
            SET token_last_used_at = now() \
           FROM ( \
                 SELECT id \
                   FROM app_user \
                  WHERE instance_host = $1 \
                    AND sync_enabled \
                    AND access_token_enc IS NOT NULL \
                    AND token_fingerprint IS NOT NULL \
                    AND deleted_at IS NULL \
                    AND token_scopes && ARRAY['read:follows', 'read', 'follow']::text[] \
                    AND NOT (token_fingerprint = ANY($2)) \
                  ORDER BY token_last_used_at ASC NULLS FIRST, id \
                  LIMIT 1 \
                    FOR UPDATE SKIP LOCKED \
                ) AS candidate \
          WHERE picked.id = candidate.id \
          RETURNING picked.id AS user_id, picked.access_token_enc, picked.token_scopes, \
                    picked.mastodon_account_id",
    )
    .bind(instance_host)
    .bind(excluded.as_slice())
    .fetch_optional(pool)
    .await
}
