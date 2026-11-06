//! One-at-a-time coordination for whole-graph work.
//!
//! Most jobs are per-item and already exclusive: a claim is a row leased with
//! `FOR UPDATE SKIP LOCKED`, so two replicas cannot work on the same user. Three
//! jobs do not fit that shape — `recompute_similarity`, `recompute_affinity` and
//! `fit_model` produce one artifact for the entire graph — and running two of
//! them at once doubles the writes and can publish a table whose rows came from
//! different inputs. Those three take this lock.
//!
//! Per-item jobs must **not** take it. Their `SKIP LOCKED` lease is already the
//! exclusion primitive, and wrapping every item in one global lock would turn
//! eight parallel workers into one for no correctness gain.
//!
//! The lock is a lease, not a mutex: it carries a TTL so a replica that dies
//! while holding it does not stop recomputes forever. Expiry can let a second
//! replica start the same job while the first is merely slow, which is why the
//! jobs are idempotent — the result is the same table, written twice.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::CacheError;

/// Proof that the holder owns a named lock, and the token that releases it.
///
/// Ownership is not "this process" but a unique `owner` string per acquisition,
/// so a guard can be checked against the stored owner: releasing a lock whose
/// TTL expired and was re-acquired by someone else must not release *their*
/// lock. A guard that is dropped without [`Lock::release`] is simply allowed to
/// expire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockGuard {
    name: String,
    owner: String,
}

impl LockGuard {
    /// Assemble a guard from the lock name and the unique owner token.
    ///
    /// Intended for lock backends; callers normally receive guards from
    /// [`Lock::acquire`].
    #[must_use]
    pub fn new(name: impl Into<String>, owner: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            owner: owner.into(),
        }
    }

    /// The lock this guard holds, without the backend's key prefix.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The acquisition token that identifies this hold.
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }
}

/// A lease over a named resource shared by all replicas.
///
/// Only the three whole-graph jobs use this. See the module docs for why
/// per-item work relies on `SKIP LOCKED` leases instead.
pub trait Lock: std::fmt::Debug + Send + Sync {
    /// Try to take `name` for `ttl`, returning `None` if another owner holds it.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the shared store cannot be reached.
    /// A caller must treat that as "someone may hold the lock" and skip the job,
    /// not as an invitation to run it unlocked.
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        ttl: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<LockGuard>, CacheError>> + Send + 'a>>;

    /// Release a lock this process still owns.
    ///
    /// Releasing an expired or foreign guard is a no-op, not an error: the TTL
    /// may have elapsed while the job was still finishing, and no correct
    /// implementation can undo the new owner's hold.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the shared store cannot be reached.
    /// The hold then survives until its TTL, which is harmless.
    fn release<'a>(
        &'a self,
        guard: &'a LockGuard,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>>;
}
