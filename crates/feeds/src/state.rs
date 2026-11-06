//! Shared request state.
//!
//! One cloneable struct handed to every handler. It holds a connection pool and read
//! only configuration; anything mutable (a verification cache, rate-limit buckets)
//! arrives later behind an explicit interior-mutability wrapper, so it is obvious
//! which parts of the state are shared beyond a single request.

use crate::config::Config;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Instant;

/// Application state, cloned per request by axum.
#[derive(Debug, Clone)]
pub struct AppState {
    /// Postgres pool. Cloning is cheap: it is an `Arc` around the pool.
    pub pool: PgPool,
    /// Boot configuration.
    pub config: Arc<Config>,
    /// When the process started, for the uptime gauge in `/readyz`.
    pub started_at: Instant,
}

impl AppState {
    /// Build state from a connected pool and a parsed configuration.
    #[must_use]
    pub fn new(pool: PgPool, config: Config) -> Self {
        Self {
            pool,
            config: Arc::new(config),
            started_at: Instant::now(),
        }
    }

    /// Seconds since this process started.
    #[must_use]
    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}
