//! The cloneable state handed to every handler.

use crate::config::Config;
use nodus_cache::Cache;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{SharedIdentityCache, VerificationCache};

/// Application state, cloned per request by axum.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Postgres pool. Cloning is cheap: it is an `Arc` around the pool.
    pub pool: PgPool,
    /// Boot configuration.
    pub config: Arc<Config>,
    /// When the process started, for the uptime gauge in `/readyz`.
    pub started_at: Instant,
    /// Outbound HTTP client, used to verify tokens and to sync on a user's behalf.
    /// One client for the process so connections and TLS sessions are reused.
    pub http: reqwest::Client,
    /// Recently verified tokens, so the hot path does not call an instance.
    ///
    /// Behind a trait because the process is expected to run as several replicas: a
    /// per-process cache stays correct when it misses, but a shared one (Redis) saves
    /// the instance round trip for every replica instead of only for one.
    pub identities: Arc<dyn VerificationCache>,
    /// The shared cache itself, for the things that are not token verifications:
    /// today only the unfurl records behind a shared post's preview.
    ///
    /// Handed out as the trait object rather than a typed wrapper because a miss is
    /// always handled by doing the real work, so there is nothing for a wrapper to
    /// guarantee.
    pub cache: Arc<dyn Cache>,
}

impl AppState {
    /// Build state from a connected pool, a parsed configuration and the shared
    /// cache backend the verified tokens are mirrored into.
    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics when the HTTP client cannot be built, which can only happen if the TLS
    /// backend is misconfigured — a condition worth failing at boot.
    pub fn new(pool: PgPool, config: Config, shared: Arc<dyn Cache>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("nodus/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("HTTP client builds");
        let identities = SharedIdentityCache::new(
            Arc::clone(&shared),
            Duration::from_secs(config.verify_cache_seconds),
            config.verify_cache_capacity,
        );
        Self {
            pool,
            config: Arc::new(config),
            started_at: Instant::now(),
            http,
            identities: Arc::new(identities),
            cache: shared,
        }
    }

    /// Seconds since this process started.
    #[must_use]
    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}
