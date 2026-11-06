//! `POST /api/v1/feedback/not-interested` — permanent, per-user suppression.
//!
//! This is not telemetry that happens to be negative: the user is telling us not to
//! show an account again. The endpoint therefore works regardless of the telemetry
//! switch, and it answers with no body — there is nothing to render and nothing the
//! client can usefully do with a receipt.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use nodus_shared::{EventKind, InteractionEvent};
use nodus_store::events::{idempotency_key, item_owned, kind_text, record};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::{Credentials, authenticate};
use crate::error::ApiError;
use crate::state::AppState;

/// Body of a not-interested report.
#[derive(Debug, Deserialize)]
pub struct NotInterestedRequest {
    /// The served item the user does not want to see again.
    pub item_id: i64,
}

/// `POST /api/v1/feedback/not-interested`
///
/// # Errors
///
/// - [`ApiError::Unauthorized`] when the token or the instance header is missing or
///   the instance rejects the token.
/// - [`ApiError::NotFound`] when the item was not served to this caller.
/// - [`ApiError::Internal`] when the write fails.
pub async fn not_interested(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NotInterestedRequest>,
) -> Result<StatusCode, ApiError> {
    let credentials = Credentials::from_headers(&headers)?;
    let caller = authenticate(&state, credentials).await?;

    if !item_owned(&state.pool, caller.user.id, body.item_id).await? {
        return Err(ApiError::NotFound);
    }

    // The suppression itself is not written here. `feed::candidates` already excludes
    // any account that has a `not_interested` (or `dismiss`) event against one of the
    // user's items, so recording the event is the whole mutation; adding a second
    // exclusion path would only give the two a chance to disagree.
    let client_ts = OffsetDateTime::now_utc();
    let event = InteractionEvent {
        item_id: body.item_id,
        kind: EventKind::NotInterested,
        client_ts: client_ts.format(&Rfc3339).unwrap_or_default(),
        dwell_ms: None,
    };
    let key = idempotency_key(body.item_id, kind_text(EventKind::NotInterested), client_ts);

    // There is no batch snapshot on this request. The event table keys on the item, not
    // on the feed, so a nil id is the honest "no snapshot" value here.
    record(&state.pool, caller.user.id, Uuid::nil(), &[(event, key)]).await?;
    Ok(StatusCode::NO_CONTENT)
}
