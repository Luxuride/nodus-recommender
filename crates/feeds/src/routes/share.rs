//! `GET /s/{instance}/{status}`: the link people paste into chats.
//!
//! Served by the API role rather than the client, because the client is a static
//! bundle on a CDN and a CDN cannot answer with per-post metadata. The page is
//! generated once and cached, so a link shared into a busy channel costs one instance
//! request per TTL rather than one per preview.

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::og;
use crate::state::AppState;

/// Longest status id worth accepting. Snowflakes are 18–20 digits; the bound exists
/// so a stranger cannot make us fetch an arbitrary path on someone else's server.
const MAX_ID_LENGTH: usize = 64;

/// How long a rendered page may be cached by an intermediary.
///
/// Shorter than the unfurl TTL: the page is cheap to rebuild and a post that is
/// deleted should stop appearing in previews within minutes.
const PAGE_MAX_AGE_SECONDS: u32 = 300;

/// Render a shared post.
pub async fn short_link(
    State(state): State<AppState>,
    Path((instance, status_id)): Path<(String, String)>,
) -> Response {
    let app_url = app_url(&state);

    if !plausible_host(&instance) || !plausible_id(&status_id) {
        return html(StatusCode::NOT_FOUND, og::page_unavailable(&app_url), 60);
    }

    let share_url = format!(
        "{}/s/{instance}/{status_id}",
        state.config.public_base_url.trim_end_matches('/')
    );

    match og::resolve(&state, &instance, &status_id).await {
        Ok(unfurl) => html(
            StatusCode::OK,
            og::page(&unfurl, &share_url, &app_url),
            PAGE_MAX_AGE_SECONDS,
        ),
        Err(error) => {
            tracing::debug!(
                instance = %instance,
                status = %status_id,
                reason = error.reason(),
                "could not unfurl a shared post"
            );
            html(
                StatusCode::NOT_FOUND,
                og::page_unavailable(&app_url),
                60,
            )
        }
    }
}

/// Where "Open in Nodus" points.
///
/// The first configured web origin is the canonical app. Empty means no client is
/// deployed next to this API, in which case the page still works: only the byline
/// links are missing a destination.
fn app_url(state: &AppState) -> String {
    state
        .config
        .web_origins
        .first()
        .cloned()
        .unwrap_or_else(|| state.config.public_base_url.clone())
}

/// A host we are willing to interpolate into a URL.
///
/// The value arrives in a path segment and ends up in an outbound request, so the
/// checks are about shape, not trust: no scheme, no path, no userinfo, no port, and
/// no characters that could re-target the request.
fn plausible_host(instance: &str) -> bool {
    !instance.is_empty()
        && instance.len() <= 255
        && instance.contains('.')
        && !instance.contains("..")
        && instance
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-'))
}

/// A status id we are willing to interpolate into a URL.
fn plausible_id(status_id: &str) -> bool {
    !status_id.is_empty()
        && status_id.len() <= MAX_ID_LENGTH
        && status_id.chars().all(|ch| ch.is_ascii_digit())
}

fn html(status: StatusCode, body: String, max_age: u32) -> Response {
    let cache = format!("public, max-age={max_age}");
    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_str(&cache).unwrap_or(HeaderValue::from_static("no-store")),
            ),
            // The page is generated from someone else's content; a strict policy
            // costs nothing here because the page carries no script at all.
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    "default-src 'none'; img-src https:; style-src 'unsafe-inline'; \
                     base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
                ),
            ),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_with_a_path_or_scheme_is_refused() {
        assert!(plausible_host("mastodon.social"));
        assert!(plausible_host("social.example.co.uk"));
        assert!(!plausible_host("https://mastodon.social"));
        assert!(!plausible_host("mastodon.social/api/v1"));
        assert!(!plausible_host("evil.example:8080"));
        assert!(!plausible_host("localhost"));
        assert!(!plausible_host(""));
    }

    #[test]
    fn only_digits_are_a_status_id() {
        assert!(plausible_id("113456789012345678"));
        assert!(!plausible_id("1/../etc"));
        assert!(!plausible_id("../1"));
        assert!(!plausible_id(""));
    }
}
