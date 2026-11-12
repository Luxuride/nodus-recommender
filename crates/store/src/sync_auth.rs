//! The service's own OAuth application, per instance, and the short-lived states that
//! tie a callback to the user who started it.
//!
//! Background sync must not borrow the web client's token (D37): the client's consent
//! is about *this browser*, while the server's is about an account it will read from
//! in the background. Two decisions, so two credentials — this module holds the second
//! one, which the user grants by authorizing Nodus itself on their instance.
//!
//! Nothing here decrypts the client secret. It is sealed with `TOKEN_ENC_KEY` like a
//! user token, and only the caller that is about to exchange a code opens it.

use sqlx::PgPool;

/// A registered application on one instance.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct InstanceApp {
    /// The public half of the credential: what goes in the authorize URL.
    pub client_id: String,
    /// The sealed half: what the token exchange needs.
    pub client_secret: Vec<u8>,
    /// The redirect URI the instance will send the user back to.
    pub redirect_uri: String,
}

/// The application registered for `instance_host`, if there is one.
///
/// # Errors
///
/// Database failures.
pub async fn app(pool: &PgPool, instance_host: &str) -> Result<Option<InstanceApp>, sqlx::Error> {
    sqlx::query_as::<_, InstanceApp>(
        "SELECT client_id, client_secret, redirect_uri FROM instance_app WHERE instance_host = $1",
    )
    .bind(instance_host)
    .fetch_optional(pool)
    .await
}

/// Records a freshly registered application.
///
/// `ON CONFLICT` replaces the row: an instance that rotated or forgot the application
/// answers `401` on the next exchange, and re-registering is the only repair. Keeping
/// the old row would make that repair impossible without a manual delete.
///
/// # Errors
///
/// Database failures.
pub async fn put_app(
    pool: &PgPool,
    instance_host: &str,
    client_id: &str,
    client_secret: &[u8],
    redirect_uri: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO instance_app (instance_host, client_id, client_secret, redirect_uri) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (instance_host) DO UPDATE SET \
             client_id = EXCLUDED.client_id, \
             client_secret = EXCLUDED.client_secret, \
             redirect_uri = EXCLUDED.redirect_uri",
    )
    .bind(instance_host)
    .bind(client_id)
    .bind(client_secret)
    .bind(redirect_uri)
    .execute(pool)
    .await?;
    Ok(())
}

/// Remembers a `state` until `expires_at`, for the user who started the authorization.
///
/// # Errors
///
/// Database failures.
pub async fn start(
    pool: &PgPool,
    state: &str,
    user_id: &uuid::Uuid,
    instance_host: &str,
    ttl_minutes: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO sync_authorization (state, user_id, instance_host, expires_at) \
         VALUES ($1, $2, $3, now() + ($4::int * interval '1 minute'))",
    )
    .bind(state)
    .bind(user_id)
    .bind(instance_host)
    .bind(ttl_minutes)
    .execute(pool)
    .await?;
    Ok(())
}

/// The user an unexpired `state` belongs to, consuming it in the same statement.
///
/// `DELETE … RETURNING` is what makes the callback single-use: a second arrival with
/// the same `state` finds nothing, even if the two requests race, because the row is
/// gone as soon as one of them deletes it.
///
/// # Errors
///
/// Database failures.
pub async fn take(pool: &PgPool, state: &str) -> Result<Option<(uuid::Uuid, String)>, sqlx::Error> {
    sqlx::query_as::<_, (uuid::Uuid, String)>(
        "DELETE FROM sync_authorization WHERE state = $1 AND expires_at > now() \
         RETURNING user_id, instance_host",
    )
    .bind(state)
    .fetch_optional(pool)
    .await
}

/// Removes states that expired, so the table cannot grow with abandoned clicks.
///
/// # Errors
///
/// Database failures.
pub async fn prune(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("DELETE FROM sync_authorization WHERE expires_at <= now()")
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}
