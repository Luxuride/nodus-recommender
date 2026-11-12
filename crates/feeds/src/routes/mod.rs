//! HTTP routing.
//!
//! The router is assembled in one place so that the middleware stack — CORS, tracing,
//! body limits — is visibly applied to *every* route. A route mounted outside
//! [`router`] would silently lose the CORS headers, which for a CDN-hosted client is
//! indistinguishable from an outage.

pub mod browse;
pub mod events;
pub mod feed;
pub mod feedback;
pub mod health;
pub mod meta;
pub mod share;
pub mod sync;
pub mod undo;

use crate::state::AppState;
use axum::Router;
use axum::http::{HeaderName, HeaderValue, Method, header};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

/// Build the application router.
///
/// # Panics
///
/// Panics only if a configured origin is not a valid header value, which
/// [`crate::config::Config`] already validated at boot.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", axum::routing::get(health::healthz))
        .route("/readyz", axum::routing::get(health::readyz))
        .route("/api/v1/meta", axum::routing::get(meta::meta))
        .route(
            "/s/{instance}/{status}",
            axum::routing::get(share::short_link),
        )
        .route("/api/v1/feed/accounts", axum::routing::get(feed::accounts))
        .route("/api/v1/feed/posts", axum::routing::get(feed::posts::posts))
        .route(
            "/api/v1/feed/{feed_id}/refresh",
            axum::routing::post(feed::refresh::refresh),
        )
        .route("/api/v1/browse", axum::routing::post(browse::browse))
        .route("/api/v1/events", axum::routing::post(events::collect))
        .route(
            "/api/v1/feedback/not-interested",
            axum::routing::post(feedback::not_interested),
        )
        .route(
            "/api/v1/feedback/{item_id}",
            axum::routing::delete(undo::lift),
        )
        .route(
            "/api/v1/me",
            axum::routing::get(sync::get_me).delete(sync::delete_me),
        )
        .route(
            "/api/v1/sync/authorize",
            axum::routing::post(sync::start_sync_authorization),
        )
        .route(
            "/api/v1/sync/callback",
            axum::routing::get(sync::sync_callback),
        )
        .route(
            "/api/v1/sync/token",
            axum::routing::put(sync::put_sync_token).delete(sync::delete_sync_token),
        )
        .layer(cors(&state))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// CORS for a separately hosted client.
///
/// Credentials are off on purpose: authentication is a bearer token, never a cookie, so
/// a browser never needs to send ambient authority. `x-contract-version` is allowed
/// because the client announces its contract on every call.
fn cors(state: &AppState) -> CorsLayer {
    let origins: Vec<HeaderValue> = state
        .config
        .web_origins
        .iter()
        .filter_map(|origin| HeaderValue::from_str(origin).ok())
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static("x-contract-version"),
            // The instance host is a header, not a path segment: one origin serves
            // tokens for every instance the user is signed into.
            HeaderName::from_static("x-instance-host"),
            HeaderName::from_static("x-request-id"),
        ])
        .max_age(std::time::Duration::from_secs(600))
}
