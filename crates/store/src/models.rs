//! Row types mirroring the schema.
//!
//! These are plain data, not entities: no behaviour, no lazy loading, no identity
//! map. A row read from the database is a snapshot, and anything that needs to be
//! consistent across two reads belongs in one query, not in a longer-lived struct.

use serde_json::Value as Json;
use time::OffsetDateTime;
use uuid::Uuid;

/// One of our users: a person who has handed us a token (or at least asked for a
/// feed).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AppUser {
    pub id: Uuid,
    /// `citext` in the database, `::text` in queries.
    pub instance_host: String,
    /// The account id on that instance. Only meaningful together with `instance_host`.
    pub mastodon_account_id: i64,
    pub acct: String,
    /// True when we hold an encrypted token and may sync in the background.
    pub sync_enabled: bool,
    pub telemetry_opt_in: bool,
    pub created_at: OffsetDateTime,
    pub last_seen_at: OffsetDateTime,
    /// Soft-deleted users keep their row for the 30-day grace period.
    pub deleted_at: Option<OffsetDateTime>,
}

/// A Mastodon account we know about, ours or a candidate's.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Account {
    pub id: i64,
    pub instance_host: String,
    pub mastodon_id: i64,
    pub remote_id: Option<String>,
    pub acct: String,
    pub display_name: Option<String>,
    pub followers_count: i32,
    pub bot: bool,
    pub locked: bool,
    pub last_fetched_at: Option<OffsetDateTime>,
    pub moved_to_account_id: Option<i64>,
}

/// A unit of background work, claimed with `FOR UPDATE SKIP LOCKED`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    /// P0 highest. See `docs/04-ingestion.md`.
    pub priority: i16,
    pub subject_kind: String,
    pub subject_id: Option<String>,
    pub payload: Json,
    pub status: String,
    pub attempts: i16,
    pub max_attempts: i16,
    /// Not before this time.
    pub run_after: OffsetDateTime,
    pub locked_at: Option<OffsetDateTime>,
    pub locked_by: Option<String>,
    pub last_error: Option<String>,
}

/// Job states as stored. Kept as `&'static str` constants so the SQL and the Rust
/// side cannot drift silently: a typo becomes a type error instead of a row that no
/// claim query will ever pick up.
pub mod job_status {
    pub const PENDING: &str = "pending";
    pub const RUNNING: &str = "running";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
}

/// Account fields we are willing to overwrite on refresh.
///
/// Counts are replaced rather than merged: a follower count can fall, and a stale
/// maximum would quietly inflate every popularity score.
#[derive(Debug, Clone)]
pub struct NewAccount {
    pub instance_host: String,
    pub mastodon_id: i64,
    pub remote_id: Option<String>,
    pub acct: String,
    pub display_name: Option<String>,
    pub followers_count: i32,
    pub bot: bool,
    pub locked: bool,
    pub discoverable: Option<bool>,
    pub indexable: Option<bool>,
    pub noindex: Option<bool>,
}
