//! Shared request state.
//!
//! One cloneable struct handed to every handler. It holds a connection pool and read
//! only configuration; anything mutable (a verification cache, rate-limit buckets)
//! arrives later behind an explicit interior-mutability wrapper, so it is obvious
//! which parts of the state are shared beyond a single request.

use crate::config::Config;
use nodus_cache::Cache;
use serde::{Deserialize, Serialize};
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
            shared,
            Duration::from_secs(config.verify_cache_seconds),
            config.verify_cache_capacity,
        );
        Self {
            pool,
            config: Arc::new(config),
            started_at: Instant::now(),
            http,
            identities: Arc::new(identities),
        }
    }

    /// Seconds since this process started.
    #[must_use]
    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}

/// What an instance told us about the bearer token's owner.
///
/// Serialized as-is into the shared cache: the field names are part of the value
/// two replicas exchange, so they are the storage format and must not be renamed
/// without invalidating every cached entry (which is safe, merely wasteful).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Verified tokens with a process-local cache in front of a shared one.
///
/// The local [`IdentityCache`] is what makes the shared backend worth having: a hot
/// token is answered from memory, and Redis is consulted only when this replica has
/// not seen the token yet. A hit in the shared layer is copied into the local one so
/// the next request on this replica pays no network round trip either.
///
/// Every shared-layer failure is treated as a miss and logged at debug. The cache is
/// an optimisation, not a source of truth: if Redis is down, the instance is asked
/// again and the request still succeeds — a cache outage must never turn into a 500.
#[derive(Debug)]
pub struct SharedIdentityCache {
    local: IdentityCache,
    shared: Arc<dyn Cache>,
    ttl: Duration,
}

impl SharedIdentityCache {
    /// Layer a local cache over `shared`, expiring entries after `ttl` and holding
    /// at most `capacity` of them per replica.
    #[must_use]
    pub fn new(shared: Arc<dyn Cache>, ttl: Duration, capacity: usize) -> Self {
        Self {
            local: IdentityCache::new(ttl, capacity),
            shared,
            ttl,
        }
    }
}

/// The shared-cache key for a token fingerprint.
///
/// Hex rather than the raw bytes because the bytes are not valid UTF-8 often
/// enough to be awkward with tooling, and because `redis-cli GET verify:<64 hex>`
/// is something an operator can type.
fn cache_key(key: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(7 + key.len() * 2);
    encoded.push_str("verify:");
    for byte in key {
        encoded.push(char::from(DIGITS[usize::from(byte >> 4)]));
        encoded.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    encoded
}

impl VerificationCache for SharedIdentityCache {
    fn get<'a>(
        &'a self,
        key: &'a [u8; 32],
    ) -> Pin<Box<dyn Future<Output = Option<Identity>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(identity) = self.local.lookup(key) {
                return Some(identity);
            }
            let encoded = cache_key(key);
            match self.shared.get(&encoded).await {
                Ok(Some(bytes)) => match serde_json::from_slice::<Identity>(&bytes) {
                    Ok(identity) => {
                        self.local.remember(*key, identity.clone());
                        Some(identity)
                    }
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            "shared verification cache held an undecodable entry"
                        );
                        None
                    }
                },
                Ok(None) => None,
                Err(error) => {
                    tracing::debug!(
                        %error,
                        "shared verification cache read failed; treating as a miss"
                    );
                    None
                }
            }
        })
    }

    fn insert<'a>(
        &'a self,
        key: [u8; 32],
        identity: Identity,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.local.remember(key, identity.clone());
            let encoded = cache_key(&key);
            let bytes = match serde_json::to_vec(&identity) {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::debug!(
                        %error,
                        "verification could not be encoded for the shared cache"
                    );
                    return;
                }
            };
            if let Err(error) = self.shared.set(&encoded, &bytes, self.ttl).await {
                tracing::debug!(%error, "shared verification cache write failed");
            }
        })
    }

    fn forget<'a>(&'a self, key: &'a [u8; 32]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.local.drop_entry(key);
            let encoded = cache_key(key);
            if let Err(error) = self.shared.delete(&encoded).await {
                tracing::debug!(%error, "shared verification cache delete failed");
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Identity, IdentityCache, SharedIdentityCache, VerificationCache};
    use nodus_cache::{Cache, CacheError};
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
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

    /// A [`Cache`] that remembers one value and counts every operation, so a test
    /// can assert a layer was or was not consulted without a Redis server or a
    /// runtime beyond the one `#[tokio::test]` provides.
    #[derive(Debug)]
    struct CountingCache {
        reads: AtomicUsize,
        writes: AtomicUsize,
        value: Mutex<Option<Vec<u8>>>,
        fail_reads: bool,
    }

    impl CountingCache {
        fn new(fail_reads: bool) -> Self {
            Self {
                reads: AtomicUsize::new(0),
                writes: AtomicUsize::new(0),
                value: Mutex::new(None),
                fail_reads,
            }
        }

        fn reads(&self) -> usize {
            self.reads.load(Ordering::Relaxed)
        }

        fn writes(&self) -> usize {
            self.writes.load(Ordering::Relaxed)
        }
    }

    impl Cache for CountingCache {
        fn get<'a>(
            &'a self,
            _key: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, CacheError>> + Send + 'a>>
        {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let answer = if self.fail_reads {
                Err(CacheError::Unavailable("test backend is down".to_owned()))
            } else {
                // A poisoned test lock is not worth distinguishing from empty.
                match self.value.lock() {
                    Ok(slot) => Ok((*slot).clone()),
                    Err(_) => Ok(None),
                }
            };
            Box::pin(std::future::ready(answer))
        }

        fn set<'a>(
            &'a self,
            _key: &'a str,
            value: &'a [u8],
            _ttl: Duration,
        ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut slot) = self.value.lock() {
                *slot = Some(value.to_vec());
            }
            Box::pin(std::future::ready(Ok(())))
        }

        fn delete<'a>(
            &'a self,
            _key: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
            if let Ok(mut slot) = self.value.lock() {
                *slot = None;
            }
            Box::pin(std::future::ready(Ok(())))
        }
    }

    fn layered(shared: Arc<CountingCache>) -> SharedIdentityCache {
        SharedIdentityCache::new(shared, Duration::from_secs(60), 4)
    }

    #[tokio::test]
    async fn a_local_hit_does_not_touch_the_shared_layer() {
        let shared = Arc::new(CountingCache::new(false));
        let cache = layered(Arc::clone(&shared));
        let key = [9_u8; 32];

        cache.insert(key, identity("a@b")).await;
        let writes_after_insert = shared.writes();
        let reads_after_insert = shared.reads();

        assert_eq!(cache.get(&key).await, Some(identity("a@b")));
        assert_eq!(cache.get(&key).await, Some(identity("a@b")));

        assert_eq!(shared.reads(), reads_after_insert, "local hit read shared");
        assert_eq!(shared.writes(), writes_after_insert, "a read wrote shared");
    }

    #[tokio::test]
    async fn a_shared_hit_populates_the_local_layer() {
        let shared = Arc::new(CountingCache::new(false));
        let cache = layered(Arc::clone(&shared));
        let key = [5_u8; 32];
        let encoded = serde_json::to_vec(&identity("shared@host")).expect("identity encodes");
        shared
            .set("verify:any", &encoded, Duration::from_secs(60))
            .await
            .expect("fake cache always writes");

        assert_eq!(cache.get(&key).await, Some(identity("shared@host")));
        assert_eq!(shared.reads(), 1);
        assert_eq!(cache.local.len(), 1, "the shared hit was not remembered");

        // The second read is answered locally, so the shared layer is not asked again.
        assert_eq!(cache.get(&key).await, Some(identity("shared@host")));
        assert_eq!(shared.reads(), 1);
    }

    #[tokio::test]
    async fn a_shared_error_is_a_miss_and_not_a_panic() {
        let shared = Arc::new(CountingCache::new(true));
        let cache = layered(Arc::clone(&shared));

        assert_eq!(cache.get(&[1_u8; 32]).await, None);
        assert_eq!(shared.reads(), 1);
        assert_eq!(
            cache.local.len(),
            0,
            "a failed read must not remember anything"
        );
    }
}
