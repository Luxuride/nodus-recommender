//! Byte caches with a typed JSON convenience.
//!
//! The cache exists to save requests, not to store anything. Every value here
//! can be recomputed from Postgres or from the instance; a miss costs latency
//! and, for the verify cache, one extra Mastodon call. That is why the API is
//! small and key-oriented: callers decide the key layout (`verify:<sha256>`,
//! `feed:<user>:<cursor>`) and the TTL, and the backend only has to make the
//! bytes visible to every replica.
//!
//! Values are bytes, not typed values, because the two users of the cache store
//! different shapes and a generic trait method would make the trait impossible
//! to hold as `dyn Cache`. The [`Cache::get_json`] and [`Cache::set_json`]
//! helpers cover the common case without changing that: they are ordinary
//! methods layered over the byte methods, available on concrete types.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// Why a cache operation could not be completed.
///
/// Both variants mean "the value is not here", never "the value is wrong". A
/// caller is expected to fall back to the source of truth, which is why the
/// enum is flat and carries no source chain: there is no recovery decision to
/// make beyond "recompute it".
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    /// The backend could not be reached or rejected the command.
    #[error("cache backend unavailable: {0}")]
    Unavailable(String),

    /// A stored value was not valid JSON for the requested type.
    #[error("cached value could not be decoded: {0}")]
    Serde(String),
}

impl From<serde_json::Error> for CacheError {
    fn from(err: serde_json::Error) -> Self {
        Self::Serde(err.to_string())
    }
}

/// A boxed future that reads a cached value.
///
/// Named because the full type appears in three signatures and
/// `clippy::type_complexity` is right that naming it once reads better than
/// spelling it out each time.
pub type GetFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, CacheError>> + Send + 'a>>;

/// A boxed future for a write or a removal, which carry no value.
pub type UnitFuture<'a> = Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>>;

/// A shared, lossy key-value store with per-entry TTLs.
///
/// Implementations must be safe to share across tasks. Writes may be visible to
/// other replicas late or not at all (a cache may drop a value under pressure),
/// so callers must always be able to rebuild what they read.
pub trait Cache: std::fmt::Debug + Send + Sync {
    /// Read `key`, returning `None` when it is absent or already expired.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the backend cannot be reached.
    fn get<'a>(&'a self, key: &'a str) -> GetFuture<'a>;

    /// Write `value` under `key`, forgetting it after `ttl`.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the backend cannot be reached.
    fn set<'a>(&'a self, key: &'a str, value: &'a [u8], ttl: Duration) -> UnitFuture<'a>;

    /// Remove `key` immediately. Removing an absent key is not an error.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the backend cannot be reached.
    fn delete<'a>(&'a self, key: &'a str) -> UnitFuture<'a>;

    /// Read and decode `key` as JSON.
    ///
    /// The `Self: Sized` bound is what keeps this generic helper out of the
    /// vtable: a `dyn Cache` can still be used for the byte methods, but the
    /// typed helpers require a concrete type (or an `impl Cache` generic).
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the backend cannot be reached, or
    /// [`CacheError::Serde`] if the stored bytes are not valid JSON for `T`.
    fn get_json<'a, T>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<T>, CacheError>> + Send + 'a>>
    where
        T: DeserializeOwned + Send + 'a,
        Self: Sized,
    {
        Box::pin(async move {
            match self.get(key).await? {
                None => Ok(None),
                Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            }
        })
    }

    /// Serialize `value` as JSON and write it under `key` for `ttl`.
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the backend cannot be reached, or
    /// [`CacheError::Serde`] if `value` cannot be serialized.
    fn set_json<'a, T>(&'a self, key: &'a str, value: &'a T, ttl: Duration) -> UnitFuture<'a>
    where
        T: Serialize + Sync + 'a,
        Self: Sized,
    {
        Box::pin(async move {
            let bytes = serde_json::to_vec(value)?;
            self.set(key, &bytes, ttl).await
        })
    }
}
