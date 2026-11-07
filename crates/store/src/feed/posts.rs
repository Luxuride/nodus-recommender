//! Post candidates: the authors a user already has affinity for, plus what they wrote.
//!
//! The account feed answers "who should I follow"; this one answers "what should I
//! read", and it is deliberately built on the same materialised affinity. A post is
//! only as good as the account behind it, so ranking starts from `account_affinity`
//! and then asks the cheaper question the status row can answer: how recent is it,
//! and how much attention did it already get.
//!
//! Two filters are structural rather than scoring: a post older than
//! [`nodus_core::post::POST_MAX_AGE_DAYS`] is not a recommendation, it is an archive,
//! and an author the user already follows does not need to be discovered through a
//! single post.

use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// One status worth ranking, with the author's affinity and size attached.
#[derive(Debug, sqlx::FromRow)]
pub struct PostCandidate {
    pub status_id: i64,
    pub account_id: i64,
    pub affinity: f64,
    pub content: String,
    pub favourites_count: i32,
    pub reblogs_count: i32,
    pub replies_count: i32,
    pub published_at: OffsetDateTime,
    pub is_reply: bool,
    pub account_acct: String,
    pub account_display_name: Option<String>,
    pub account_avatar_url: Option<String>,
    pub account_followers_count: i32,
    pub instance_host: String,
}

/// Post candidates for one user, strongest affinity first.
///
/// `LIMIT` is applied before ranking, so the caller asks for more than it will show
/// (the page planner needs spare rows to choose from). Ordering by affinity rather
/// than by recency is what keeps the pool a *taste* pool: recency decides the score
/// later, but an old strong match still beats a fresh stranger.
pub async fn candidates(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<PostCandidate>, sqlx::Error> {
    sqlx::query_as::<_, PostCandidate>(
        r"
        WITH me AS (
            SELECT a.id AS account_id
            FROM app_user u
            JOIN account a
              ON a.instance_host = u.instance_host
             AND a.mastodon_id = u.mastodon_account_id
            WHERE u.id = $1
        )
        SELECT
            s.id                                                   AS status_id,
            s.account_id                                           AS account_id,
            af.affinity::double precision                          AS affinity,
            coalesce(s.content, '')                                AS content,
            s.favourites_count,
            s.reblogs_count,
            s.replies_count,
            s.created_at                                           AS published_at,
            s.in_reply_to_id IS NOT NULL                           AS is_reply,
            a.acct::text                                           AS account_acct,
            a.display_name                                         AS account_display_name,
            a.avatar_url                                           AS account_avatar_url,
            a.followers_count                                      AS account_followers_count,
            a.instance_host::text                                  AS instance_host
        FROM account_affinity af
        JOIN account a ON a.id = af.account_id
        JOIN status s ON s.account_id = af.account_id
        JOIN me ON true
        WHERE af.user_id = $1
          AND af.expires_at > now()
          AND s.visibility IN ('public', 'unlisted')
          AND s.expires_at > now()
          AND s.created_at > now() - make_interval(days => $3)
          AND a.moved_to_account_id IS NULL
          AND NOT EXISTS (
              SELECT 1 FROM follow_edge fe
              WHERE fe.follower_id = me.account_id
                AND fe.followee_id = a.id
          )
          AND NOT EXISTS (
              SELECT 1
              FROM interaction_event e
              JOIN recommendation_item ri ON ri.id = e.item_id
              WHERE e.user_id = $1
                AND ri.account_id = a.id
                AND e.kind IN ('dismiss', 'not_interested')
          )
        ORDER BY af.affinity DESC, s.created_at DESC
        LIMIT $2
        ",
    )
    .bind(user_id)
    .bind(limit)
    .bind(i64::from(nodus_core::post::POST_MAX_AGE_DAYS))
    .fetch_all(pool)
    .await
}

/// A scored post, ready to be minted as a `recommendation_item`.
#[derive(Debug)]
pub struct NewPostItem {
    pub status_id: i64,
    pub account_id: i64,
    pub score: f64,
    pub reason: Value,
}

/// Records the snapshot and returns `(item_id, status_id)` pairs.
///
/// The caller must pair ids by position with the input order; the insert preserves it
/// because `UNNEST` walks the arrays in step, and `RETURNING` reports rows in
/// insertion order.
pub async fn persist(
    pool: &PgPool,
    user_id: Uuid,
    feed_id: Uuid,
    model_version: i32,
    items: &[NewPostItem],
) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let statuses = items.iter().map(|item| item.status_id).collect::<Vec<_>>();
    let accounts = items.iter().map(|item| item.account_id).collect::<Vec<_>>();
    let scores = items.iter().map(|item| item.score).collect::<Vec<_>>();
    let reasons = items
        .iter()
        .map(|item| item.reason.clone())
        .collect::<Vec<_>>();

    sqlx::query_as::<_, (i64, i64)>(
        r"
        INSERT INTO recommendation_item
            (user_id, feed_id, kind, status_id, account_id, score, reason, model_version)
        SELECT $1, $2, 'post', s.status_id, s.account_id, s.score, s.reason, $3
        FROM UNNEST($4::bigint[], $5::bigint[], $6::float8[], $7::jsonb[])
            AS s(status_id, account_id, score, reason)
        RETURNING id, status_id
        ",
    )
    .bind(user_id)
    .bind(feed_id)
    .bind(model_version)
    .bind(&statuses)
    .bind(&accounts)
    .bind(&scores)
    .bind(&reasons)
    .fetch_all(pool)
    .await
}

/// One row of a post snapshot, read back by cursor.
#[derive(Debug, sqlx::FromRow)]
pub struct SnapshotPost {
    pub id: i64,
    pub score: f32,
    pub reason: Value,
    pub status_id: i64,
    pub account_acct: String,
    pub account_display_name: Option<String>,
    pub account_avatar_url: Option<String>,
    pub content: String,
    pub published_at: OffsetDateTime,
    pub favourites_count: i32,
    pub reblogs_count: i32,
    pub replies_count: i32,
    pub instance_host: String,
}

/// Reads a page of an existing post snapshot.
pub async fn page(
    pool: &PgPool,
    user_id: Uuid,
    feed_id: Uuid,
    after_item_id: i64,
    limit: i64,
) -> Result<Vec<SnapshotPost>, sqlx::Error> {
    sqlx::query_as::<_, SnapshotPost>(
        r"
        SELECT
            ri.id                                                  AS id,
            ri.score                                               AS score,
            ri.reason                                              AS reason,
            ri.status_id                                           AS status_id,
            a.acct::text                                           AS account_acct,
            a.display_name                                         AS account_display_name,
            a.avatar_url                                           AS account_avatar_url,
            coalesce(s.content, '')                                AS content,
            s.created_at                                           AS published_at,
            s.favourites_count,
            s.reblogs_count,
            s.replies_count,
            a.instance_host::text                                  AS instance_host
        FROM recommendation_item ri
        JOIN status s ON s.id = ri.status_id
        JOIN account a ON a.id = ri.account_id
        WHERE ri.user_id = $1
          AND ri.feed_id = $2
          AND ri.kind = 'post'
          AND ri.id > $3
        ORDER BY ri.id
        LIMIT $4
        ",
    )
    .bind(user_id)
    .bind(feed_id)
    .bind(after_item_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}
