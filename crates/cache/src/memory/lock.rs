//! In-process locks behind [`Lock`](crate::lock::Lock), exclusive by name and TTL.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::cache::CacheError;
use crate::lock::{Lock, LockGuard};

use super::poisoned;

/// In-process exclusive locks with TTL expiry.
///
/// Only correct within one process. More than one replica needs the Redis
/// backend, because two independent maps cannot see each other's holds.
#[derive(Debug, Default)]
pub struct InMemoryLock {
    held: Mutex<HashMap<String, Held>>,
}

#[derive(Debug, Clone)]
struct Held {
    owner: String,
    /// `None` when the TTL overflowed an `Instant`; such a hold never expires.
    expires_at: Option<Instant>,
}

impl Held {
    fn is_live(&self, now: Instant) -> bool {
        self.expires_at.is_none_or(|expires_at| expires_at > now)
    }
}

/// Distinguishes two acquisitions in the same process, and two processes on the
/// same machine, without pulling in a UUID dependency. Only the owner string has
/// to be unique, not globally sortable.
static NEXT_OWNER: AtomicU64 = AtomicU64::new(1);

fn next_owner() -> String {
    let sequence = NEXT_OWNER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{sequence}", std::process::id())
}

impl InMemoryLock {
    /// Create an empty lock table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn table(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, Held>>, CacheError> {
        self.held.lock().map_err(|_| poisoned("lock"))
    }

    /// Try to take `name` for `ttl` at `now`.
    ///
    /// An expired hold is treated as absent, which is the lease semantics the
    /// trait promises. The explicit instant is the test seam; production code
    /// goes through [`Lock::acquire`].
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the lock table is poisoned.
    pub fn acquire_at(
        &self,
        name: &str,
        ttl: Duration,
        now: Instant,
    ) -> Result<Option<LockGuard>, CacheError> {
        let mut held = self.table()?;
        if held.get(name).is_some_and(|entry| entry.is_live(now)) {
            return Ok(None);
        }
        let owner = next_owner();
        held.insert(
            name.to_owned(),
            Held {
                owner: owner.clone(),
                expires_at: now.checked_add(ttl),
            },
        );
        Ok(Some(LockGuard::new(name, owner)))
    }

    /// Release `guard`, but only if its owner token is still the stored one.
    ///
    /// A stale guard — one whose TTL expired and was re-acquired — leaves the
    /// new hold untouched. This is the same compare-and-delete the shared
    /// backend performs, expressed as a single map operation.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the lock table is poisoned.
    pub fn unlock(&self, guard: &LockGuard) -> Result<(), CacheError> {
        let mut held = self.table()?;
        if held
            .get(guard.name())
            .is_some_and(|entry| entry.owner == guard.owner())
        {
            held.remove(guard.name());
        }
        Ok(())
    }
}

impl Lock for InMemoryLock {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        ttl: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<LockGuard>, CacheError>> + Send + 'a>> {
        Box::pin(async move { self.acquire_at(name, ttl, Instant::now()) })
    }

    fn release<'a>(
        &'a self,
        guard: &'a LockGuard,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        Box::pin(async move { self.unlock(guard) })
    }
}
