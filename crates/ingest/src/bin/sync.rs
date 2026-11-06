//! Background sync worker.
//!
//! Owns every outbound Mastodon request that is not made by a user's browser:
//! following lists, favourites, bookmarks, recent posts, candidate refreshes.
//! The job queue is a Postgres table with `FOR UPDATE SKIP LOCKED`; there is no
//! broker. Rate limiting is per host and self-imposed below the reported budget
//! — see `docs/04-ingestion.md`.
//!
//! Unimplemented by design: the scheduling and budget rules are specified in
//! `docs/04-ingestion.md` before the worker exists.

#![forbid(unsafe_code)]

fn main() {
    println!("mastodon-reccom-sync {}", env!("CARGO_PKG_VERSION"));
    println!("No worker implemented yet — see docs/04-ingestion.md.");
}
