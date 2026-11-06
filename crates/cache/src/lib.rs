//! Ephemeral shared state: the parts replicas must agree on but must never
//! treat as durable.
//!
//! Postgres is the system of record. Everything in this crate is the opposite
//! kind of storage: a budget that keeps two calculator replicas from spending
//! the same instance's rate limit, a lock that keeps them from recomputing the
//! whole graph twice, and a cache that saves a Mastodon round trip. All of it
//! is shared *between* replicas and all of it is disposable.
//!
//! That split is the whole reason the crate exists. The rule that makes it safe
//! is loss: if every key here vanished at once, the service would still be
//! correct. A budget resets to full and the crawler is briefly more eager, a
//! cache misses and one request is repeated, a lock expires and a second replica
//! starts the same recompute — wasteful, slower, never wrong. Nothing that
//! cannot be rebuilt from Postgres belongs behind these traits.
//!
//! Two backends implement the traits. [`memory`] keeps everything in process
//! maps, which is exact for a single replica and for tests, and is what the
//! service uses when no `REDIS_URL` is configured. The `redis` feature adds
//! [`redis_backend`], which is what makes the state shared across processes; it
//! is optional so the rest of the workspace can depend on this crate without
//! pulling in a Redis client.
//!
//! The traits return boxed futures rather than being `async fn`s because callers
//! hold them as `dyn Budget` / `dyn Cache`; an `async fn` in a trait is not
//! object-safe, and the extra allocation is irrelevant next to the network hop.

#![forbid(unsafe_code)]

pub mod budget;
pub mod cache;
pub mod lock;
pub mod memory;
#[cfg(feature = "redis")]
pub mod redis_backend;

pub use budget::Budget;
pub use cache::{Cache, CacheError};
pub use lock::{Lock, LockGuard};
pub use memory::{InMemoryBudget, InMemoryCache, InMemoryLock};
#[cfg(feature = "redis")]
pub use redis_backend::{RedisBudget, RedisCache};
