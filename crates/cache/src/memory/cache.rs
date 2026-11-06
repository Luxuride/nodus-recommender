//! The in-process byte cache behind [`Cache`](crate::cache::Cache).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::cache::{Cache, CacheError};

use super::poisoned;

/// In-process byte cache with TTLs.
///
/// Unlike the budget and the lock this is a pure convenience: a single replica
/// could keep these bytes in a field, and nothing here is larger than the feed
/// and token payloads the service already handles.
#[derive(Debug, Default)]
pub struct InMemoryCache {
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug, Clone)]
struct Entry {
    value: Vec<u8>,
    /// `None` when the TTL overflowed an `Instant`; such an entry never expires.
    expires_at: Option<Instant>,
}

impl Entry {
    fn is_live(&self, now: Instant) -> bool {
        self.expires_at.is_none_or(|expires_at| expires_at > now)
    }
}

impl InMemoryCache {
    /// Create an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn table(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Entry>>, CacheError> {
        self.entries.lock().map_err(|_| poisoned("cache"))
    }

    /// Read `key` at `now`, dropping it first if it has expired.
    ///
    /// The explicit instant is the test seam; production code goes through
    /// [`Cache::get`].
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the cache map is poisoned.
    pub fn get_at(&self, key: &str, now: Instant) -> Result<Option<Vec<u8>>, CacheError> {
        let mut entries = self.table()?;
        let expired = entries.get(key).is_some_and(|entry| !entry.is_live(now));
        if expired {
            entries.remove(key);
            return Ok(None);
        }
        Ok(entries.get(key).map(|entry| entry.value.clone()))
    }

    /// Write `value` under `key` until `now + ttl`.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the cache map is poisoned.
    pub fn set_at(
        &self,
        key: &str,
        value: &[u8],
        ttl: Duration,
        now: Instant,
    ) -> Result<(), CacheError> {
        let mut entries = self.table()?;
        entries.insert(
            key.to_owned(),
            Entry {
                value: value.to_vec(),
                expires_at: now.checked_add(ttl),
            },
        );
        Ok(())
    }

    /// Forget `key` immediately.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the cache map is poisoned.
    pub fn remove(&self, key: &str) -> Result<(), CacheError> {
        let mut entries = self.table()?;
        entries.remove(key);
        Ok(())
    }
}

impl Cache for InMemoryCache {
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, CacheError>> + Send + 'a>> {
        Box::pin(async move { self.get_at(key, Instant::now()) })
    }

    fn set<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        Box::pin(async move { self.set_at(key, value, ttl, Instant::now()) })
    }

    fn delete<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        Box::pin(async move { self.remove(key) })
    }
}
