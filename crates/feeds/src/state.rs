//! Shared request state.
//!
//! One cloneable struct handed to every handler. It holds a connection pool and read
//! only configuration; anything mutable (a verification cache, rate-limit buckets)
//! arrives later behind an explicit interior-mutability wrapper, so it is obvious
//! which parts of the state are shared beyond a single request.

use crate::config::Config;
use sqlx::PgPool;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
}

impl AppState {
    /// Build state from a connected pool and a parsed configuration.
    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics when the HTTP client cannot be built, which can only happen if the TLS
    /// backend is misconfigured — a condition worth failing at boot.
    pub fn new(pool: PgPool, config: Config) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("nodus/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("HTTP client builds");
        Self {
            pool,
            config: Arc::new(config),
            started_at: Instant::now(),
            http,
            identities: Arc::new(IdentityCache::new(Duration::from_secs(600), 10_000)),
        }
    }

    /// Seconds since this process started.
    #[must_use]
    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

/// What an instance told us about the bearer token's owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The account's numeric id at its own instance.
    pub mastodon_account_id: i64,
    /// `user@host` as the instance spells it.
    pub acct: String,
}

/// A cache of verified tokens, keyed by their fingerprint.
///
/// The methods are asynchronous so a shared implementation can be dropped in without
/// changing a single call site: the local map answers immediately, Redis waits.
pub trait VerificationCache: std::fmt::Debug + Send + Sync {
    /// Look up a fingerprint, returning `None` when it is absent or expired.
    ///
    /// Boxed rather than `impl Future` so the trait stays object safe: the state holds
    /// `Arc<dyn VerificationCache>` precisely so a shared implementation can replace
    /// the local map without touching handlers.
    fn get<'a>(
        &'a self,
        key: &'a [u8; 32],
    ) -> Pin<Box<dyn Future<Output = Option<Identity>> + Send + 'a>>;

    /// Remember a verification result.
    fn insert<'a>(
        &'a self,
        key: [u8; 32],
        identity: Identity,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// Forget a fingerprint, for instance because the instance revoked the token.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the 401 path calls this once revocation is wired")
    )]
    fn forget<'a>(&'a self, key: &'a [u8; 32]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// Bounded, time-limited cache of verified tokens.
///
/// A `Mutex` rather than a sharded map: verification happens once per token per ten
/// minutes, so contention is irrelevant next to the network call it saves. The
/// capacity bound is what matters — an unbounded map keyed by attacker-supplied
/// tokens is a memory leak with extra steps.
#[derive(Debug)]
pub struct IdentityCache {
    ttl: Duration,
    capacity: usize,
    entries: Mutex<HashMap<[u8; 32], (Instant, Identity)>>,
}

impl IdentityCache {
    /// Build a cache with a time-to-live and a hard entry cap.
    #[must_use]
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            ttl,
            capacity,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn lookup(&self, key: &[u8; 32]) -> Option<Identity> {
        let mut entries = self.entries.lock().ok()?;
        match entries.get(key) {
            Some((inserted_at, identity)) if inserted_at.elapsed() < self.ttl => {
                Some(identity.clone())
            }
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    fn remember(&self, key: [u8; 32], identity: Identity) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.retain(|_, (inserted_at, _)| inserted_at.elapsed() < self.ttl);
        if entries.len() >= self.capacity && !entries.contains_key(&key) {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (inserted_at, _))| *inserted_at)
                .map(|(key, _)| *key)
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(key, (Instant::now(), identity));
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "reached through the trait's forget")
    )]
    fn drop_entry(&self, key: &[u8; 32]) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(key);
        }
    }

    /// Number of entries currently held, expired ones included.
    #[must_use]
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "reported by the metrics line later")
    )]
    pub fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or_default()
    }
}

impl VerificationCache for IdentityCache {
    fn get<'a>(
        &'a self,
        key: &'a [u8; 32],
    ) -> Pin<Box<dyn Future<Output = Option<Identity>> + Send + 'a>> {
        Box::pin(std::future::ready(self.lookup(key)))
    }

    fn insert<'a>(
        &'a self,
        key: [u8; 32],
        identity: Identity,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        self.remember(key, identity);
        Box::pin(std::future::ready(()))
    }

    fn forget<'a>(&'a self, key: &'a [u8; 32]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        self.drop_entry(key);
        Box::pin(std::future::ready(()))
    }
}

#[cfg(test)]
mod tests {
    use super::{Identity, IdentityCache, VerificationCache};
    use std::time::Duration;

    /// Drive a ready future to completion without a runtime.
    fn poll<F: std::future::Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(output) => output,
            std::task::Poll::Pending => panic!("the local cache never pends"),
        }
    }

    fn identity(acct: &str) -> Identity {
        Identity {
            mastodon_account_id: 1,
            acct: acct.to_owned(),
        }
    }

    #[test]
    fn an_inserted_identity_is_returned() {
        let cache = IdentityCache::new(Duration::from_secs(60), 4);
        let key = [7_u8; 32];
        poll(cache.insert(key, identity("a@b")));
        assert_eq!(poll(cache.get(&key)), Some(identity("a@b")));
    }

    #[test]
    fn an_expired_identity_is_not_returned() {
        let cache = IdentityCache::new(Duration::from_millis(1), 4);
        let key = [7_u8; 32];
        poll(cache.insert(key, identity("a@b")));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(poll(cache.get(&key)), None);
    }

    #[test]
    fn the_cache_never_exceeds_its_capacity() {
        let cache = IdentityCache::new(Duration::from_secs(60), 2);
        for byte in 0..5_u8 {
            poll(cache.insert([byte; 32], identity("a@b")));
        }
        assert!(cache.len() <= 2, "len was {}", cache.len());
    }

    #[test]
    fn a_forgotten_identity_is_gone() {
        let cache = IdentityCache::new(Duration::from_secs(60), 4);
        let key = [3_u8; 32];
        poll(cache.insert(key, identity("a@b")));
        poll(cache.forget(&key));
        assert_eq!(poll(cache.get(&key)), None);
    }
}
