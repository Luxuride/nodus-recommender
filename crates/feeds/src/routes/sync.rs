//! `PUT` and `DELETE /api/v1/sync/token` — the consent surface.
//!
//! Background sync is the only thing in this service that holds a secret, so it is
//! the only thing a user has to opt into explicitly, and the only thing they can
//! switch off with one call that provably destroys the copy we hold. Telemetry is a
//! separate switch on purpose: a user may want their feed improved without wanting
//! their clicks stored.

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use nodus_shared::{MeResponse, SyncSettings, SyncTokenRequest};

use crate::auth::{Credentials, authenticate};
use crate::error::ApiError;
use crate::state::AppState;
use nodus_store::crypto::fingerprint;

/// Start holding an encrypted copy of the caller's token and queue their first sync.
///
/// # Errors
///
/// [`ApiError::Unavailable`] with `sync_disabled` when this deployment has no
/// encryption key, `instance_unreachable`/`instance_rate_limited` when the token
/// cannot be verified, and [`ApiError::Unauthorized`] when the instance rejects it.
pub async fn put_sync_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SyncTokenRequest>,
) -> Result<Json<SyncSettings>, ApiError> {
    let credentials = Credentials::from_headers(&headers)?;
    if !body.enable {
        return stop_sync(&state, credentials, body.telemetry_opt_in).await;
    }
    let sealer = state
        .config
        .token_enc_key
        .as_ref()
        .ok_or_else(|| ApiError::unavailable("sync_disabled", 3600))?;
    let caller = authenticate(&state, credentials).await?;
    let encrypted = sealer
        .seal(caller.credentials.token.expose())
        .map_err(|error| {
            tracing::error!(error = %error, "sealing a token failed");
            ApiError::Internal(crate::error::anyhow_detail::Detail(error.to_string()))
        })?;
    let token_fingerprint = fingerprint(caller.credentials.token.expose());
    let scopes: Vec<String> = Vec::new();
    let stored = nodus_store::users::set_sync_token(
        &state.pool,
        caller.user.id,
        &encrypted,
        &scopes,
        &token_fingerprint,
    )
    .await?;
    if !stored {
        return Err(ApiError::NotFound);
    }
    nodus_store::users::set_telemetry_opt_in(&state.pool, caller.user.id, body.telemetry_opt_in)
        .await?;
    nodus_store::jobs::enqueue(
        &state.pool,
        &nodus_store::jobs::NewJob::sync_user(&caller.user.id),
    )
    .await?;
    tracing::info!(user = %caller.user.acct, "background sync enabled");
    Ok(Json(SyncSettings {
        sync_enabled: true,
        telemetry_opt_in: body.telemetry_opt_in,
    }))
}

/// Stop holding the token and forget what was derived from it.
///
/// The feed does not stop working: it degrades to what can be computed from the
/// caller's own token on each request.
///
/// # Errors
///
/// [`ApiError::Unauthorized`] when the instance rejects the token,
/// [`ApiError::Internal`] for database failures.
pub async fn delete_sync_token(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SyncSettings>, ApiError> {
    let credentials = Credentials::from_headers(&headers)?;
    stop_sync(&state, credentials, false).await
}

/// `GET /api/v1/me` — who the service thinks the caller is, and what it holds.
///
/// # Errors
///
/// [`ApiError::Unauthorized`] when the token is rejected, [`ApiError::Internal`] for
/// database failures.
pub async fn get_me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<MeResponse>, ApiError> {
    let credentials = Credentials::from_headers(&headers)?;
    let caller = authenticate(&state, credentials).await?;
    Ok(Json(MeResponse {
        acct: caller.user.acct.clone(),
        instance_host: caller.user.instance_host.clone(),
        sync_enabled: caller.user.sync_enabled,
        telemetry_opt_in: caller.user.telemetry_opt_in,
        contract: nodus_shared::CONTRACT_VERSION.to_owned(),
    }))
}

/// `DELETE /api/v1/me` — forget this user entirely.
///
/// # Errors
///
/// [`ApiError::Unauthorized`] when the token is rejected, [`ApiError::Internal`] for
/// database failures.
pub async fn delete_me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SyncSettings>, ApiError> {
    let credentials = Credentials::from_headers(&headers)?;
    let caller = authenticate(&state, credentials).await?;
    nodus_store::users::clear_sync_token(&state.pool, caller.user.id).await?;
    nodus_store::users::soft_delete(&state.pool, caller.user.id).await?;
    tracing::info!(user = %caller.user.acct, "user deleted");
    Ok(Json(SyncSettings {
        sync_enabled: false,
        telemetry_opt_in: false,
    }))
}

async fn stop_sync(
    state: &AppState,
    credentials: Credentials,
    telemetry_opt_in: bool,
) -> Result<Json<SyncSettings>, ApiError> {
    let caller = authenticate(state, credentials).await?;
    nodus_store::users::clear_sync_token(&state.pool, caller.user.id).await?;
    if !telemetry_opt_in {
        nodus_store::users::set_telemetry_opt_in(&state.pool, caller.user.id, false).await?;
    }
    tracing::info!(user = %caller.user.acct, "background sync disabled");
    Ok(Json(SyncSettings {
        sync_enabled: false,
        telemetry_opt_in: telemetry_opt_in && caller.user.telemetry_opt_in,
    }))
}
