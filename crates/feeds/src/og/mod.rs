//! Turn a status into something a browser or a chat client can render.
//!
//! A shared post link has two audiences and they need different things. A chat
//! client fetches the URL once and looks only at the `<meta>` tags, so it must
//! receive real HTML with the post already described. A person clicking the link
//! also wants the post, and wants it without installing anything.
//!
//! Both are served from the same page: a small HTML document with the tags at the
//! top and a readable rendering of the status below. No JavaScript, no wasm, no
//! hydration — a crawler that refuses to run scripts and a browser with scripts
//! disabled see the same thing, which is also the reason the page is safe to cache
//! and safe to serve from a role that has no idea what the client is doing.
//!
//! The instance is asked for the status with no token. A status that is not public
//! therefore cannot be unfurled, and the page says so without confirming whether
//! the post exists.

mod render;
mod unfurl;

use std::sync::Arc;
use std::time::Duration;

use nodus_cache::Cache;

use crate::state::AppState;

pub use render::{page, unavailable as page_unavailable};
pub use unfurl::{Unfurl, UnfurlError};

/// Cache key prefix. Versioned because a change in the shape of [`Unfurl`] must
/// not be read back by a replica running the previous build during a rolling
/// deploy: the `v1` bump makes old and new entries distinct.
const KEY_PREFIX: &str = "og:v1";

/// How long a failed unfurl is remembered.
///
/// Short, because a post that is private today may be public later and a chat
/// client retrying a link must not be told "unavailable" for a day. Still long
/// enough that a dead link pasted into a busy channel is fetched once.
const NEGATIVE_TTL: Duration = Duration::from_secs(300);

/// Resolve a status into an [`Unfurl`], going through the shared cache.
///
/// The cache is what keeps a popular link cheap: a post shared into a large
/// channel is fetched from its instance once per TTL instead of once per preview.
/// Every cache failure is treated as a miss, so a cache outage costs latency and
/// instance requests, never correctness.
///
/// # Errors
///
/// Returns [`UnfurlError`] when the status cannot be fetched or is not public.
pub async fn resolve(state: &AppState, host: &str, status_id: &str) -> Result<Unfurl, UnfurlError> {
    let key = cache_key(host, status_id);
    if let Some(cached) = read(&state.cache, &key).await {
        return cached;
    }
    let outcome = unfurl::fetch(&state.http, host, status_id).await;
    match &outcome {
        Ok(unfurl) => write(&state.cache, &key, unfurl, state.config.og_cache_seconds).await,
        Err(error) => write_failure(&state.cache, &key, error).await,
    }
    outcome
}

/// `og:v1:<host>:<status id>`.
fn cache_key(host: &str, status_id: &str) -> String {
    format!("{KEY_PREFIX}:{host}:{status_id}")
}

/// A cached result, success or remembered failure.
async fn read(cache: &Arc<dyn Cache>, key: &str) -> Option<Result<Unfurl, UnfurlError>> {
    let bytes = cache.get(key).await.ok().flatten()?;
    match serde_json::from_slice::<Cached>(&bytes) {
        Ok(Cached::Found(unfurl)) => Some(Ok(unfurl)),
        Ok(Cached::Missing { reason }) => Some(Err(UnfurlError::from_reason(&reason))),
        // A value this build cannot read is a value from another build or another
        // service sharing the namespace. Treat it as absent rather than guessing.
        Err(_) => None,
    }
}

async fn write(cache: &Arc<dyn Cache>, key: &str, unfurl: &Unfurl, seconds: u64) {
    let value = Cached::Found(unfurl.clone());
    store(cache, key, &value, Duration::from_secs(seconds)).await;
}

async fn write_failure(cache: &Arc<dyn Cache>, key: &str, error: &UnfurlError) {
    let value = Cached::Missing {
        reason: error.reason().to_owned(),
    };
    store(cache, key, &value, NEGATIVE_TTL).await;
}

async fn store(cache: &Arc<dyn Cache>, key: &str, value: &Cached, ttl: Duration) {
    let Ok(bytes) = serde_json::to_vec(value) else {
        // Serialization of our own type cannot fail for these fields; if it ever
        // does, the caller keeps its answer and the cache stays empty.
        return;
    };
    let _ = cache.set(key, &bytes, ttl).await;
}

/// What lives under one cache key.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum Cached {
    Found(Unfurl),
    Missing { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_namespaced_by_version_and_host() {
        assert_eq!(
            cache_key("mastodon.social", "123"),
            "og:v1:mastodon.social:123"
        );
        // Two instances can use the same snowflake for different posts, so the host
        // has to be part of the key.
        assert_ne!(cache_key("a.example", "1"), cache_key("b.example", "1"));
    }

    #[test]
    fn a_remembered_failure_survives_a_round_trip() {
        let value = Cached::Missing {
            reason: "not_found".to_owned(),
        };
        let bytes = serde_json::to_vec(&value).expect("serializes");
        let Ok(Cached::Missing { reason }) = serde_json::from_slice(&bytes) else {
            panic!("expected a remembered failure");
        };
        assert_eq!(reason, "not_found");
        assert!(matches!(
            UnfurlError::from_reason(&reason),
            UnfurlError::NotFound
        ));
    }
}
