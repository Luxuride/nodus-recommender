//! The calculator: everything in the recommendation service that grows with the
//! graph.
//!
//! It owns the outbound instance traffic (following lists, favourites, bookmarks,
//! recent posts), the similarity and affinity recomputes, model fitting and
//! retention. Nothing here serves user traffic — that is `nodus-feeds` — and the
//! two roles never call each other: the `job` table is the hand-off, so either
//! can be restarted alone. See `docs/02-architecture.md`.
//!
//! M replicas are safe because claims use `FOR UPDATE SKIP LOCKED` and a lease
//! deadline re-queues whatever a dying replica held. What M replicas must *share*
//! — per-host budgets and the one-at-a-time global recompute — lives in Redis,
//! which is why running with more than one replica without `REDIS_URL` is a
//! configuration error rather than a silent multiplication of the request rate.

#![forbid(unsafe_code)]

pub mod config;
pub mod handlers;
pub mod worker;

pub use config::{Config, ConfigError};
pub use handlers::{Handler, JobError, Registry};
pub use worker::Worker;
