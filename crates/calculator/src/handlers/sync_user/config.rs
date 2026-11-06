//! Pacing of one crawl.
//!
//! [`SyncConfig`] is the whole knob surface the calculator hands a handler, and
//! [`BURST`] fixes the in-process budget's starting depth. Nothing here performs
//! I/O; the values are consumed by [`super::SyncUser::new`] and by the token loop.

use std::sync::Arc;

use nodus_cache::Budget;
use nodus_store::crypto::TokenSealer;

/// Burst for the in-process budget: one second's worth of requests can go at once,
/// which is what lets a first page of 80 accounts arrive without stalling.
pub(super) const BURST: u32 = 5;

/// How a crawl behaves: request spacing, page size and a hard page cap.
///
/// The cap is not paranoia — one account following 20 000 people would otherwise
/// spend the whole instance budget on a single user. When it trips, the graph is
/// truncated and logged, and the next run starts from the top of the list again.
#[derive(Debug, Clone)]
pub struct SyncConfig {
    /// The sealing key, shared with `nodus-feeds`.
    pub sealer: Arc<TokenSealer>,
    /// Accounts per request; Mastodon caps this at 80.
    pub page_size: u16,
    /// Hard cap on pages per run.
    pub max_pages: usize,
    /// The configured rate, from `SYNC_MAX_RPS_PER_HOST`.
    pub requests_per_second: f64,
    /// The per-host request budget, once one is shared (Redis); `None` means the
    /// handler builds the in-process one on construction.
    ///
    /// It lives here rather than in a new state struct because `SyncConfig` is
    /// already what `Registry::standard` hands to every handler, and this port is
    /// the only shared state a crawl needs beyond the pool. A struct wrapping
    /// `pool` + `config` would add a layer without changing what is shared.
    pub budget: Option<Arc<dyn Budget>>,
    /// Scheme used to reach an instance: `https`, except for a development instance
    /// that serves plain HTTP on localhost.
    pub scheme: String,
}

impl SyncConfig {
    /// A config with Mastodon's documented page size and our default pacing.
    #[must_use]
    pub fn new(sealer: Arc<TokenSealer>, max_rps: f64) -> Self {
        let rps = if max_rps > 0.0 { max_rps } else { 1.0 };
        Self {
            sealer,
            page_size: 80,
            max_pages: 250,
            requests_per_second: rps,
            budget: None,
            scheme: "https".to_owned(),
        }
    }

    /// Override the page size, for an instance whose cap is lower than the default.
    #[must_use]
    pub const fn with_page_size(mut self, page_size: u16) -> Self {
        self.page_size = page_size;
        self
    }

    /// Talk to instances over `scheme` instead of `https`.
    ///
    /// Only a development instance serving plain HTTP needs this; federation in
    /// production is HTTPS-only, and a scheme that could be set per user would be a
    /// way to point our token at an attacker's server.
    #[must_use]
    pub fn with_scheme(mut self, scheme: impl Into<String>) -> Self {
        self.scheme = scheme.into();
        self
    }

    /// Use `budget` as the shared per-host request budget.
    ///
    /// Without this the handler falls back to [`nodus_cache::InMemoryBudget`], which is correct
    /// for one replica and wrong for several: each replica would get its own bucket
    /// and the effective rate would be the configured rate times the replica count.
    #[must_use]
    pub fn with_budget(mut self, budget: Arc<dyn Budget>) -> Self {
        self.budget = Some(budget);
        self
    }

    /// The configured rate as whole requests per second, for the budget backends.
    ///
    /// Both implementations take `u32`, while the configured rate is a fraction
    /// (0.5 means one request every two seconds). The bucket is sized by the ceiling
    /// and the token's own spend pattern supplies the fractional pacing.
    /// Float-to-int casts saturate in Rust, so a huge rate lands at `u32::MAX`,
    /// `NaN` at 0, and the floor of one keeps the bucket usable.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[must_use]
    pub fn budget_rate(&self) -> u32 {
        (self.requests_per_second.ceil() as u32).max(1)
    }
}
