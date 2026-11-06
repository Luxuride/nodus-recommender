//! `POST /api/v1/events` — batched interaction telemetry.
//!
//! Batching exists so a client that scrolled through a screen sends one request rather
//! than thirty; the endpoint is idempotent, so the client may retry a batch freely
//! without inflating the counts the ranking model is trained on. The whole batch is
//! rejected when it is too large or malformed, because a partially applied batch is
//! harder to reason about than a retried one.
//!
//! Consent is enforced here rather than negotiated with the client: a user who has not
//! opted into telemetry gets `204` and their events are discarded, so the client has no
//! branch to get wrong and opt-out remains a server-side property.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use nodus_shared::{EventBatch, MIN_CONTRACT_VERSION};
use nodus_store::events::{idempotency_key, kind_text, record};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::auth::{Credentials, authenticate};
use crate::error::ApiError;
use crate::state::AppState;

/// Largest batch a single request may carry.
const MAX_EVENTS: usize = 100;

/// `POST /api/v1/events`
///
/// Takes an [`EventBatch`] and answers `204 No Content`; the response carries nothing
/// the client can act on, and in particular does not say how many events were stored.
///
/// # Errors
///
/// - [`ApiError::ClientTooOld`] when the caller's contract is below
///   [`MIN_CONTRACT_VERSION`].
/// - [`ApiError::Unauthorized`] when the token or the instance header is missing or
///   the instance rejects the token.
/// - [`ApiError::BadRequest`] with `batch_too_large` above `MAX_EVENTS`, with
///   `invalid_feed_id` for a feed id this service did not mint, or with
///   `invalid_client_ts` for a timestamp that is not RFC 3339.
/// - [`ApiError::Internal`] when the write fails.
pub async fn collect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(batch): Json<EventBatch>,
) -> Result<StatusCode, ApiError> {
    contract_gate(&headers)?;
    let credentials = Credentials::from_headers(&headers)?;
    let caller = authenticate(&state, credentials).await?;

    // Opted out: accept and discard. The response is identical to the stored case so
    // the client cannot (and need not) behave differently.
    if !caller.user.telemetry_opt_in {
        return Ok(StatusCode::NO_CONTENT);
    }

    if batch.events.len() > MAX_EVENTS {
        return Err(ApiError::bad_request(
            "batch_too_large",
            format!("a batch may carry at most {MAX_EVENTS} events"),
        ));
    }

    let feed_id = Uuid::parse_str(&batch.feed_id).map_err(|_| {
        ApiError::bad_request("invalid_feed_id", "feed_id was not issued by this service")
    })?;

    let mut prepared = Vec::with_capacity(batch.events.len());
    for event in batch.events {
        let client_ts = OffsetDateTime::parse(&event.client_ts, &Rfc3339).map_err(|_| {
            ApiError::bad_request(
                "invalid_client_ts",
                "client_ts must be an RFC 3339 timestamp",
            )
        })?;
        let key = idempotency_key(event.item_id, kind_text(event.kind), client_ts);
        prepared.push((event, key));
    }

    record(&state.pool, caller.user.id, feed_id, &prepared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Rejects a client whose contract version predates ours.
///
/// A missing header is allowed: curl and the first integration tests do not send one.
/// A header we cannot understand is not.
fn contract_gate(headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(client) = headers
        .get("x-contract-version")
        .and_then(|value| value.to_str().ok())
    else {
        return Ok(());
    };
    let client = client.trim();
    if version(client) < version(MIN_CONTRACT_VERSION) {
        return Err(ApiError::ClientTooOld {
            client: client.to_owned(),
            min: MIN_CONTRACT_VERSION.to_owned(),
        });
    }
    Ok(())
}

/// Parses `major.minor.patch`, ignoring anything after the patch number.
fn version(text: &str) -> (u32, u32, u32) {
    let mut parts = text.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    });
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(version("0.2.0") > version("0.1.9"));
        assert_eq!(version("0.1.0-rc1"), (0, 1, 0));
    }

    #[test]
    fn a_missing_header_passes_the_gate() {
        assert!(contract_gate(&HeaderMap::new()).is_ok());
    }

    #[test]
    fn an_older_client_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert("x-contract-version", "0.0.1".parse().unwrap());
        assert!(matches!(
            contract_gate(&headers),
            Err(ApiError::ClientTooOld { .. })
        ));
    }
}
