//! `GET /api/v1/meta`.
//!
//! The handshake. A client calls this once at boot to learn what this server can do
//! before it renders anything: the contract version it should assume, the oldest
//! version still honoured, and the optional capabilities it may gate UI on. Feature
//! flags are additive by construction — a client that does not know a flag ignores it.

use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use nodus_shared::{CONTRACT_VERSION, MIN_CONTRACT_VERSION, Meta};

/// Capabilities this build implements.
///
/// Names are part of the contract: rename one and old clients lose the feature.
pub const FEATURES: [&str; 3] = ["feed_accounts", "events", "sync_token"];

/// Report contract version and capabilities.
pub async fn meta(State(_state): State<AppState>) -> Response {
    let body = Meta {
        api: "1".to_owned(),
        contract: CONTRACT_VERSION.to_owned(),
        min_contract: MIN_CONTRACT_VERSION.to_owned(),
        features: FEATURES.iter().map(|f| (*f).to_owned()).collect(),
    };
    // Safe to cache for a minute: the payload changes only on deploy, and a stale copy
    // can at worst delay a new feature flag by that minute.
    ([(header::CACHE_CONTROL, "public, max-age=60")], Json(body)).into_response()
}
