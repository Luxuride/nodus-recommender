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

use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// One candidate as the calculator left it in `account_affinity`, joined with the
/// account row the feed needs to render.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Candidate {
    /// Local account id.
    pub account_id: i64,
    /// Damped, normalised affinity.
    pub affinity: f64,
    /// Number of similar users supporting the candidate.
    pub support: i16,
    /// Which propagation sources produced it.
    pub sources: Vec<String>,
    /// When the affinity was computed.
    pub computed_at: OffsetDateTime,
    /// When this user was last shown this account, if ever.
    ///
    /// Feeds the stability term: an account the user saw an hour ago keeps a
    /// small bonus, so a refresh does not reshuffle a list the user is still
    /// reading. `None` means never shown, which earns no bonus.
    pub last_shown_at: Option<OffsetDateTime>,
    /// Impressions of this account in the last 7 days.
    ///
    /// The fatigue term, and the reason a user who has scrolled past an account
    /// four times without tapping it stops seeing it: exposure without interaction
    /// is a negative signal, not a neutral one.
    pub seen_count_7d: i32,
    /// `user` or `user@host`.
    pub acct: String,
    /// Display name, when the account set one.
    pub display_name: Option<String>,
    /// Plain-text note (HTML stripped by the calcutor's ingest path).
    pub note: Option<String>,
    /// Absolute avatar URL.
    pub avatar_url: Option<String>,
    /// Follower count at the last refresh.
    pub followers_count: i32,
    /// Whether the account is marked as a bot.
    pub bot: bool,
    /// Last public post time, when known.
    pub last_status_at: Option<OffsetDateTime>,
    /// The candidate's instance, for the same-instance ranking term.
    pub instance_host: String,
}

/// Candidates for one user, best first, with the exclusions the feed must apply.
///
/// Excluded are: accounts the user already follows (a recommendation to follow
/// someone you follow is not a recommendation), accounts they dismissed or marked
/// not interested, expired affinity, and accounts whose owner said they are not
/// discoverable.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn candidates(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<Candidate>, sqlx::Error> {
    sqlx::query_as::<_, Candidate>(
        "WITH me AS ( \
             SELECT a.id AS account_id, a.instance_host \
             FROM app_user u \
             JOIN account a ON a.instance_host = u.instance_host \
                 AND a.mastodon_id = u.mastodon_account_id \
             WHERE u.id = $1 \
         ) \
         SELECT af.account_id, af.affinity::float8 AS affinity, af.support, af.sources, \
             af.computed_at, shown.served_at AS last_shown_at, \
             impressions.seen AS seen_count_7d, \
             a.acct::text AS acct, \
             a.display_name, a.note, a.avatar_url, a.followers_count, a.bot, \
             a.last_status_at, a.instance_host::text AS instance_host \
         FROM account_affinity af \
         JOIN account a ON a.id = af.account_id \
         LEFT JOIN me ON true \
         LEFT JOIN LATERAL ( \
             SELECT ri.served_at FROM recommendation_item ri \
             WHERE ri.user_id = $1 AND ri.account_id = af.account_id \
             ORDER BY ri.served_at DESC LIMIT 1 \
         ) AS shown ON true \
         LEFT JOIN LATERAL ( \
             SELECT count(*)::integer AS seen \
             FROM interaction_event e \
             JOIN recommendation_item ri ON ri.id = e.item_id \
             WHERE e.user_id = $1 AND ri.account_id = af.account_id \
                 AND e.kind = 'impression' \
                 AND e.server_ts > now() - interval '7 days' \
         ) AS impressions ON true \
         WHERE af.user_id = $1 \
             AND af.expires_at > now() \
             AND a.moved_to_account_id IS NULL \
             AND a.discoverable IS DISTINCT FROM false \
             AND NOT EXISTS ( \
                 SELECT 1 FROM follow_edge f \
                 WHERE f.follower_id = me.account_id AND f.followee_id = af.account_id \
             ) \
             AND NOT EXISTS ( \
                 SELECT 1 FROM interaction_event e \
                 JOIN recommendation_item ri ON ri.id = e.item_id \
                 WHERE e.user_id = $1 \
                     AND ri.account_id = af.account_id \
                     AND e.kind IN ('dismiss', 'not_interested') \
             ) \
         ORDER BY af.affinity DESC, af.account_id \
         LIMIT $2",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

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
            seen_count_7d: 0,
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
