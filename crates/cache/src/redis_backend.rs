//! Redis-backed shared state.
//!
//! These implementations are what let more than one replica share the keys
//! described in `docs/02-architecture.md`: a per-host counter that every
//! calculator replica decrements, and cache entries that either service can
//! read. Without them each replica would police its own rate and the fleet would
//! make N times the requests the policy allows.
//!
//! Everything here is behind the `redis` feature, and every failure maps to
//! [`CacheError::Unavailable`]. Nothing in this module retries: Redis is
//! allowed to be down, and the caller decides whether to degrade (skip the
//! cache, reschedule the job) or to surface the error. A retry loop inside a
//! request path would turn a degraded dependency into a stalled service.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use redis::AsyncCommands;
use redis::aio::ConnectionManager;

use crate::{Budget, Cache, CacheError};

impl From<redis::RedisError> for CacheError {
    fn from(err: redis::RedisError) -> Self {
        Self::Unavailable(err.to_string())
    }
}

/// A [`Cache`] over a shared Redis connection.
///
/// `ConnectionManager` is cloned per operation: it is an `Arc` around a
/// multiplexed connection, so a clone is a pointer copy and commands from
/// different replicas (and tasks) are pipelined on one socket. The connection is
/// re-established in the background by the manager when it drops, which is why
/// there is no reconnect logic here.
///
/// Keys are used verbatim, so the caller owns the documented layout
/// (`verify:<sha256>`, `feed:<user>:<cursor>`). No prefix is added: a prefix
/// would have to match across both services, and getting it out of sync would
/// silently split the cache into two disjoint namespaces.
pub struct RedisCache {
    conn: ConnectionManager,
}

impl fmt::Debug for RedisCache {
    // ConnectionManager deliberately does not implement Debug (it holds socket
    // state, not a description), so report the backend without its internals.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("RedisCache").finish_non_exhaustive()
    }
}

impl RedisCache {
    /// Connect to the instance at `url` (for example `redis://127.0.0.1:6379`).
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the URL is invalid or the initial
    /// connection cannot be established. The manager reconnects after later
    /// failures, but it cannot start without one successful handshake.
    pub async fn connect(url: &str) -> Result<Self, CacheError> {
        let client = redis::Client::open(url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self { conn })
    }

    /// Wrap an existing connection manager, for callers that share one across
    /// the cache, the budget and anything else.
    #[must_use]
    pub fn from_manager(conn: ConnectionManager) -> Self {
        Self { conn }
    }
}

impl Cache for RedisCache {
    fn get<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, CacheError>> + Send + 'a>> {
        Box::pin(async move {
            let mut conn = self.conn.clone();
            let value: Option<Vec<u8>> = conn.get(key).await?;
            Ok(value)
        })
    }

    fn set<'a>(
        &'a self,
        key: &'a str,
        value: &'a [u8],
        ttl: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        Box::pin(async move {
            let mut conn = self.conn.clone();
            // Redis expiry is a whole number of seconds; rounding a sub-second
            // TTL down to zero would make `SET ... EX 0` an error (and, worse,
            // an immediate expiry), so round up to the minimum Redis accepts.
            let seconds = ttl.as_secs().max(1);
            let _: () = conn.set_ex(key, value, seconds).await?;
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        Box::pin(async move {
            let mut conn = self.conn.clone();
            let _: i64 = conn.del(key).await?;
            Ok(())
        })
    }
}

/// A [`Budget`] shared by all replicas, counted per host, token and second.
///
/// This is intentionally not a token bucket. The shared version is one `INCR`
/// against `budget:<host>:<bucket>:<unix-second>` with a two-second expiry,
/// compared against the configured rate: cheap, atomic, and easy to reason about
/// when something looks wrong in Redis. The caller names the bucket (the crawler
/// passes a token fingerprint), so a spent token does not silence its siblings.
/// It is **approximate and deliberately simple** — a token bucket implemented as a
/// Lua script is a later refinement, once there is evidence that the per-second
/// window is too coarse. Until then the burst is effectively one second's worth of
/// tokens, and a replica that sees a `false` answer reschedules rather than
/// retries.
///
/// A host-wide hold lives at `budget:<host>:hold` with a TTL, which is how an
/// instance's `Retry-After` becomes state every replica respects. The acquire
/// reads it first, so one `429` parks every token on that host, not just the one
/// that received it — two round trips instead of one, in exchange for not walking
/// into the same refusal.
///
/// The two-second expiry (rather than one) tolerates clock skew and the moment
/// between `INCR` and `EXPIRE`; a key that misses its expiry is bounded by
/// hosts × buckets × seconds and is harmless. The 50%-of-reported-remaining rule
/// is not enforced here because the headers are not visible to this type; the
/// crawler applies it on top of the answer.
pub struct RedisBudget {
    conn: ConnectionManager,
    rate_per_second: u64,
}

/// The longest a single `Retry-After` may park a host, mirroring the in-process
/// budget's cap.
const MAX_HOLD: Duration = Duration::from_secs(86_400);

fn hold_key(host: &str) -> String {
    format!("budget:{host}:hold")
}

impl fmt::Debug for RedisBudget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RedisBudget")
            .field("rate_per_second", &self.rate_per_second)
            .finish_non_exhaustive()
    }
}

impl RedisBudget {
    /// Wrap an existing connection manager and a per-host rate.
    #[must_use]
    pub fn new(conn: ConnectionManager, rate_per_second: u32) -> Self {
        Self {
            conn,
            rate_per_second: u64::from(rate_per_second),
        }
    }

    /// Connect to the instance at `url` (for example `redis://127.0.0.1:6379`).
    ///
    /// # Errors
    /// Returns [`CacheError::Unavailable`] if the URL is invalid or the initial
    /// connection cannot be established.
    pub async fn connect(url: &str, rate_per_second: u32) -> Result<Self, CacheError> {
        let client = redis::Client::open(url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self::new(conn, rate_per_second))
    }
}

impl Budget for RedisBudget {
    fn acquire<'a>(
        &'a self,
        host: &'a str,
        bucket: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<bool, CacheError>> + Send + 'a>> {
        Box::pin(async move {
            let mut conn = self.conn.clone();
            // A live hold refuses every token on the host; checking it first is
            // what makes one `Retry-After` apply to the whole pool.
            let held: Option<u8> = conn.get(hold_key(host)).await?;
            if held.is_some() {
                return Ok(false);
            }
            // A clock before the epoch is not worth failing a request over;
            // the second bucket would just be wrong for one tick.
            let second = match SystemTime::now().duration_since(UNIX_EPOCH) {
                Ok(elapsed) => elapsed.as_secs(),
                Err(_) => 0,
            };
            let key = format!("budget:{host}:{bucket}:{second}");
            let count: u64 = conn.incr(&key, 1i64).await?;
            if count == 1 {
                // Only the first request in this second needs to set the TTL;
                // re-setting it would extend the key's life on every request.
                let _: bool = conn.expire(&key, 2i64).await?;
            }
            Ok(count <= self.rate_per_second)
        })
    }

    fn hold<'a>(
        &'a self,
        host: &'a str,
        hold: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<(), CacheError>> + Send + 'a>> {
        Box::pin(async move {
            let mut conn = self.conn.clone();
            let seconds = hold.min(MAX_HOLD).as_secs().max(1);
            let _: () = conn.set_ex(hold_key(host), 1_u8, seconds).await?;
            Ok(())
        })
    }

    fn retry_hint(&self, _host: &str, _bucket: &str) -> Duration {
        // Redis cannot answer this without another round trip, and the caller only
        // needs a floor that keeps a refused job from spinning.
        let rate = u32::try_from(self.rate_per_second.max(1)).unwrap_or(u32::MAX);
        Duration::from_secs(1) / rate
    }
}
