//! Reads and writes for the feed path.
//!
//! This is the only module where `nodus-feeds` writes to the database, and the only
//! table it writes is `recommendation_item` — minting item ids is inherently the job
//! of whoever serves the feed, because an impression has to point at something that
//! existed when the user saw it.
//!
//! Everything else here is a read: affinity the calculator materialised, the
//! exclusions that make a recommendation worth showing, and the snapshot a second
//! page reads back.
//!
//! The directory keeps the two reads apart: [`candidate`] holds the row the feed
//! ranks and the affinity query that produces it, [`popular`] holds the cold-start
//! rung that produces the same row from the instance's follower counts, and this
//! file holds the snapshot a second page reads back. Everything is re-exported here,
//! so `feed::…` callers did not move.

use sqlx::{PgPool, postgres::types::PgInterval};
use time::OffsetDateTime;
use uuid::Uuid;

mod attention;
mod candidate;
mod popular;
pub mod posts;

pub use attention::baseline_ms;
pub use candidate::{Candidate, candidates};
pub use popular::popular_candidates;

/// One item about to be served.
#[derive(Debug, Clone)]
pub struct NewItem {
    /// Local account id.
    pub account_id: i64,
    /// Final ranking score.
    pub score: f64,
    /// Explanation, stored so a later page can render the same reason.
    pub reason: serde_json::Value,
}

/// Persists a snapshot and returns `(item_id, account_id)` in insertion order.
///
/// One statement for the whole page: the feed's write path must not turn into N round
/// trips when a page is 50 items.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn persist(
    pool: &PgPool,
    user_id: Uuid,
    feed_id: Uuid,
    model_version: i32,
    items: &[NewItem],
) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let accounts: Vec<i64> = items.iter().map(|item| item.account_id).collect();
    let scores: Vec<f64> = items.iter().map(|item| item.score).collect();
    let reasons: Vec<serde_json::Value> = items.iter().map(|item| item.reason.clone()).collect();

    sqlx::query_as::<_, (i64, i64)>(
        "INSERT INTO recommendation_item \
             (user_id, account_id, kind, score, reason, model_version, feed_id) \
         SELECT $1, account_id, 'account', score, reason, $2, $3 \
         FROM UNNEST($4::bigint[], $5::float8[], $6::jsonb[]) \
             AS page (account_id, score, reason) \
         RETURNING id, account_id",
    )
    .bind(user_id)
    .bind(model_version)
    .bind(feed_id)
    .bind(&accounts)
    .bind(&scores)
    .bind(&reasons)
    .fetch_all(pool)
    .await
}

/// A page of an existing snapshot, read back by item id.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SnapshotItem {
    /// Item id.
    pub id: i64,
    /// Final score.
    pub score: f64,
    /// Stored explanation.
    pub reason: serde_json::Value,
    /// Local account id.
    pub account_id: i64,
    /// `user` or `user@host`.
    pub acct: String,
    /// Display name.
    pub display_name: Option<String>,
    /// Plain-text note.
    pub note: Option<String>,
    /// Absolute avatar URL.
    pub avatar_url: Option<String>,
    /// Follower count.
    pub followers_count: i32,
    /// Bot flag.
    pub bot: bool,
    /// Last public post time.
    pub last_status_at: Option<OffsetDateTime>,
    /// The candidate's instance.
    pub instance_host: String,
}

/// Reads the next page of a snapshot, oldest item id last served as the cursor.
///
/// Keyset by `id` rather than by score: scores can be re-ranked between pages, and a
/// reader scrolling through a list expects it not to reshuffle underneath them.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn page(
    pool: &PgPool,
    user_id: Uuid,
    feed_id: Uuid,
    after_item_id: i64,
    limit: i64,
) -> Result<Vec<SnapshotItem>, sqlx::Error> {
    sqlx::query_as::<_, SnapshotItem>(
        "SELECT ri.id, ri.score::float8 AS score, ri.reason, ri.account_id, \
             a.acct::text AS acct, a.display_name, a.note, a.avatar_url, a.followers_count, \
             a.bot, a.last_status_at, a.instance_host::text AS instance_host \
         FROM recommendation_item ri \
         JOIN account a ON a.id = ri.account_id \
         WHERE ri.user_id = $1 AND ri.feed_id = $2 AND ri.id > $3 \
         ORDER BY ri.id \
         LIMIT $4",
    )
    .bind(user_id)
    .bind(feed_id)
    .bind(after_item_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// How old the newest snapshot for this user is, when one is still live.
///
/// One row, newest first: the feed is served from a snapshot, and the only question
/// the route asks here is "is there something to reuse". A snapshot's age is taken
/// from its newest item because every item of a snapshot is written in one
/// statement and therefore shares a `served_at`.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn live_snapshot(
    pool: &PgPool,
    user_id: Uuid,
    max_age: PgInterval,
) -> Result<Option<(Uuid, f64)>, sqlx::Error> {
    // `age_seconds` keeps the comparison in SQL: a client clock and a replica clock
    // disagree, and only the database's `now()` is the one the rows were stamped with.
    sqlx::query_as::<_, (Uuid, f64)>(
        "SELECT feed_id, EXTRACT(EPOCH FROM (now() - max(served_at)))::float8 AS age_seconds \
         FROM recommendation_item \
         WHERE user_id = $1 AND feed_id IS NOT NULL AND served_at > now() - $2 \
         GROUP BY feed_id \
         ORDER BY max(served_at) DESC \
         LIMIT 1",
    )
    .bind(user_id)
    .bind(max_age)
    .fetch_optional(pool)
    .await
}

/// Determines whether a snapshot may still be continued, and for how long it has existed.
///
/// `Some(age_seconds)` means the snapshot exists and is younger than `max_age`;
/// `Some` with a larger age means the client is holding a cursor into an expired
/// feed and must be told to start again (410), rather than silently receiving the
/// tail of a ranking the model has since replaced. `None` means the snapshot does
/// not exist at all, which is a different answer: the client's cursor is bogus.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn snapshot_age(
    pool: &PgPool,
    user_id: Uuid,
    feed_id: Uuid,
) -> Result<Option<f64>, sqlx::Error> {
    // `max(served_at)` over zero rows is NULL, and aggregating always yields exactly
    // one row, so the `None` case arrives as a NULL value rather than as a missing row.
    sqlx::query_scalar::<_, Option<f64>>(
        "SELECT (EXTRACT(EPOCH FROM (now() - max(served_at))))::float8 \
         FROM recommendation_item \
         WHERE user_id = $1 AND feed_id = $2",
    )
    .bind(user_id)
    .bind(feed_id)
    .fetch_one(pool)
    .await
}

/// The active ranking model version, or `0` when none has been promoted yet.
///
/// Zero is a real answer here: it means "hand-tuned weights", which is what M1 ships
/// with, and it keeps every item's `model_version` comparable without a nullable
/// column.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn active_model_version(pool: &PgPool) -> Result<i32, sqlx::Error> {
    let version = sqlx::query_scalar::<_, i32>(
        "SELECT version FROM model_version WHERE active ORDER BY version DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    Ok(version.unwrap_or(0))
}

/// The age in hours of the newest similarity snapshot for a user, if any.
///
/// The feed reports this rather than hiding it: a recommendation built on a
/// week-old graph is still worth showing, but the client should be able to say so.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn similarity_age_hours(
    pool: &PgPool,
    user_id: Uuid,
) -> Result<Option<f64>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<f64>>(
        "SELECT (EXTRACT(EPOCH FROM (now() - max(computed_at))) / 3600.0)::float8 \
         FROM user_similarity WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_candidate_carries_the_fields_the_feed_renders() {
        // Compile-time guard: adding a required field to the DTO must touch this
        // struct, so the query and the response cannot drift without a build error.
        let candidate = Candidate {
            account_id: 1,
            affinity: 0.5,
            support: 2,
            sources: vec!["similar_user".to_owned()],
            computed_at: OffsetDateTime::UNIX_EPOCH,
            last_shown_at: None,
            seen_attention_7d: 0.0,
            rejected_kind: None,
            rejected_at: None,
            impressions: 0,
            follow_through_shown: None,
            follow_through_followed: None,
            expected_follow_rate: None,
            acct: "a@b".to_owned(),
            display_name: None,
            note: None,
            avatar_url: None,
            followers_count: 0,
            bot: false,
            last_status_at: None,
            instance_host: "b".to_owned(),
        };
        assert_eq!(candidate.support, 2);
    }
}
