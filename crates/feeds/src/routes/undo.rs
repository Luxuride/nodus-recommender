//! `DELETE /api/v1/feedback/{item_id}` — undo a rejection.
//!
//! A `dismiss` or `not_interested` is a command, not an estimate (D27, D26), so decay
//! only softens it: without this endpoint a user who changes their mind has to wait
//! out a half-life before the account returns at full strength. The endpoint is the
//! explicit correction.
//!
//! `204` and `404` are deliberately different answers. `204` means something was
//! actually removed; `404` means there was nothing to remove, which lets the client
//! tell the user "nothing to undo" instead of showing a success for a no-op.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use nodus_store::feedback;

use crate::auth::{Credentials, authenticate};
use crate::error::ApiError;
use crate::state::AppState;

/// `DELETE /api/v1/feedback/{item_id}`
///
/// # Errors
///
/// - [`ApiError::Unauthorized`] when the token or the instance header is missing or
///   the instance rejects the token.
/// - [`ApiError::BadRequest`] with `invalid_item_id` when the path segment is not an
///   `i64`.
/// - [`ApiError::NotFound`] when the item has no `dismiss`/`not_interested` event to
///   lift; the wire code is `not_found`.
/// - [`ApiError::Internal`] when the delete fails.
pub async fn lift(
    State(state): State<AppState>,
    Path(item_id): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let credentials = Credentials::from_headers(&headers)?;
    let caller = authenticate(&state, credentials).await?;

    let item_id = parse_item_id(&item_id)?;
    let Some(lifted) = feedback::lift(&state.pool, caller.user.id, item_id).await? else {
        return Err(ApiError::NotFound);
    };

    tracing::debug!(
        item_id = lifted.item_id,
        kind = %lifted.kind,
        user_id = %caller.user.id,
        "rejection lifted"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Parses the path segment ourselves so a bad id gets our error envelope.
///
/// Letting axum's `Path<i64>` reject the request would answer with its own plain-text
/// `400` and skip the `{"error": {"code", ...}}` shape every other failure uses.
///
/// # Errors
///
/// - [`ApiError::BadRequest`] with `invalid_item_id` when `raw` is not a decimal `i64`.
pub(super) fn parse_item_id(raw: &str) -> Result<i64, ApiError> {
    raw.parse::<i64>()
        .map_err(|_| ApiError::bad_request("invalid_item_id", "item_id must be a 64-bit integer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decimal_segment_is_accepted() {
        assert_eq!(parse_item_id("918273").ok(), Some(918_273));
    }

    #[test]
    fn trailing_garbage_is_a_bad_request() {
        let error = parse_item_id("918273x").err();
        assert!(matches!(
            error,
            Some(ApiError::BadRequest {
                code: "invalid_item_id",
                ..
            })
        ));
    }

    #[test]
    fn an_out_of_range_segment_is_a_bad_request() {
        let error = parse_item_id("99999999999999999999").err();
        assert!(matches!(
            error,
            Some(ApiError::BadRequest {
                code: "invalid_item_id",
                ..
            })
        ));
    }
}
