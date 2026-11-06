//! The request's query parameters, the limits they are clamped to, and the guards
//! that run before any row is read.

use crate::error::ApiError;
use axum::http::HeaderMap;
use nodus_shared::MIN_CONTRACT_VERSION;
use serde::Deserialize;

/// Query parameters of a feed request.
#[derive(Debug, Deserialize)]
pub struct FeedParams {
    /// Page size; clamped to [`MIN_LIMIT`]`..=`[`MAX_LIMIT`].
    pub(super) limit: Option<i64>,
    /// Cursor returned by the previous page.
    pub(super) cursor: Option<String>,
}

/// Smallest page the endpoint will serve.
pub(super) const MIN_LIMIT: i64 = 1;
/// Largest page the endpoint will serve.
pub(super) const MAX_LIMIT: i64 = 100;
/// Page size when the client does not ask for one.
pub(super) const DEFAULT_LIMIT: i64 = 50;

/// Rejects a client whose contract version predates ours.
pub(super) fn contract_gate(headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(client) = headers
        .get("x-contract-version")
        .and_then(|value| value.to_str().ok())
    else {
        // A missing header is allowed: curl and the first integration tests do not send
        // one. A header we cannot understand is not.
        return Ok(());
    };
    let client = client.trim();
    if parse_version(client) < parse_version(MIN_CONTRACT_VERSION) {
        return Err(ApiError::ClientTooOld {
            client: client.to_owned(),
            min: MIN_CONTRACT_VERSION.to_owned(),
        });
    }
    Ok(())
}

/// Parses `major.minor.patch`, ignoring anything after the patch number.
pub(super) fn parse_version(text: &str) -> (u32, u32, u32) {
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

/// Why the feed is not fully personalised, if it is not.
pub(super) fn degraded_flag(sync_enabled: bool, age_hours: Option<f64>) -> Option<String> {
    if !sync_enabled {
        return Some("no_server_sync".to_owned());
    }
    match age_hours {
        None => Some("cold_start".to_owned()),
        Some(hours) if hours > 48.0 => Some("stale_similarity".to_owned()),
        Some(_) => None,
    }
}

/// Splits a `limit + 1` row read into a page and "there is more".
pub(super) fn split<T>(mut rows: Vec<T>, limit: i64) -> (Vec<T>, bool) {
    let more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    if more {
        rows.truncate(usize::try_from(limit).unwrap_or(0));
    }
    (rows, more)
}
