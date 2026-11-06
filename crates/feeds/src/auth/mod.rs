//! Who is calling, and how do we know.
//!
//! Nodus holds no accounts of its own. The caller proves who they are by
//! presenting the access token they already have for their instance, and we learn
//! their identity by asking that instance. A token is opaque, so the request also
//! has to say *which* instance to ask — hence the `X-Instance-Host` header.
//!
//! Verification is cached for ten minutes keyed by the token's fingerprint. That is
//! the compromise between authenticity (an instance call per request would triple
//! our latency and spend the user's rate limit) and revocation (a revoked token is
//! honoured for at most ten minutes, and any 401 from the instance clears the entry
//! immediately).
//!
//! [`credentials`] parses what the request presents (`Authorization` and
//! `X-Instance-Host`), [`headers`] names the header and validates the host it
//! carries, and this file verifies the token against the instance and resolves it
//! to a `app_user` row. The two public types and the header constant are
//! re-exported here so `crate::auth::…` paths keep working.

use axum::http::header;
use serde::Deserialize;

use crate::error::ApiError;
use crate::state::{AppState, Identity};
use nodus_store::crypto::fingerprint;

mod credentials;
mod headers;

pub use credentials::Credentials;

/// A verified caller: their representation in our database.
#[derive(Debug)]
pub struct Authenticated {
    /// Row in `app_user`, created on first sight.
    pub user: nodus_store::models::AppUser,
    /// The credentials the request presented, for handlers that echo settings back.
    pub credentials: Credentials,
}

/// Verify the presented token and resolve it to a user row.
///
/// # Errors
///
/// [`ApiError::Unauthorized`] when the instance rejects the token,
/// [`ApiError::Unavailable`] when the instance cannot be reached or is rate limiting
/// us, and [`ApiError::Internal`] for database failures.
pub async fn authenticate(
    state: &AppState,
    credentials: Credentials,
) -> Result<Authenticated, ApiError> {
    let key = fingerprint(credentials.token.expose());
    let identity = if let Some(cached) = state.identities.get(&key).await {
        cached
    } else {
        let verified = verify_at_instance(state, &credentials).await?;
        state.identities.insert(key, verified.clone()).await;
        verified
    };
    nodus_store::users::ensure_instance(&state.pool, &credentials.instance_host).await?;
    let user = nodus_store::users::upsert(
        &state.pool,
        &credentials.instance_host,
        identity.mastodon_account_id,
        &identity.acct,
    )
    .await?;
    Ok(Authenticated { user, credentials })
}

#[derive(Debug, Deserialize)]
struct VerifyResponse {
    /// Mastodon sends account ids as strings; treating them as numbers is exactly the
    /// kind of thing to get wrong when a field happens to be numeric today.
    id: String,
    acct: String,
}

async fn verify_at_instance(
    state: &AppState,
    credentials: &Credentials,
) -> Result<Identity, ApiError> {
    let url = format!(
        "https://{}/api/v1/accounts/verify_credentials",
        credentials.instance_host
    );
    let response = state
        .http
        .get(&url)
        .bearer_auth(credentials.token.expose())
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                host = %credentials.instance_host,
                error = %error,
                "instance unreachable while verifying a token"
            );
            ApiError::unavailable("instance_unreachable", 30)
        })?;

    match response.status().as_u16() {
        200 => {
            let body: VerifyResponse = response.json().await.map_err(|error| {
                tracing::warn!(
                    host = %credentials.instance_host,
                    error = %error,
                    "instance returned an unreadable verify_credentials body"
                );
                ApiError::unavailable("instance_unreachable", 30)
            })?;
            let mashdoton_account_id = body.id.parse::<i64>().map_err(|_| {
                tracing::warn!(
                    host = %credentials.instance_host,
                    id = %body.id,
                    "instance returned a non-numeric account id"
                );
                ApiError::unavailable("instance_unreachable", 30)
            })?;
            Ok(Identity {
                mastodon_account_id: mashdoton_account_id,
                acct: body.acct,
            })
        }
        401 | 403 | 404 => Err(ApiError::Unauthorized),
        429 => {
            let retry_after_s = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(30);
            Err(ApiError::unavailable(
                "instance_rate_limited",
                retry_after_s,
            ))
        }
        status => {
            tracing::warn!(
                host = %credentials.instance_host,
                status,
                "instance failed while verifying a token"
            );
            Err(ApiError::unavailable("instance_unreachable", 30))
        }
    }
}

#[cfg(test)]
mod tests;
