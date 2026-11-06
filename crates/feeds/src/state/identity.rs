//! Verified tokens: the wire shape, the trait the state holds, and the local map.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

    pub(super) fn lookup(&self, key: &[u8; 32]) -> Option<Identity> {
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

    pub(super) fn remember(&self, key: [u8; 32], identity: Identity) {
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

    pub(super) fn drop_entry(&self, key: &[u8; 32]) {
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
