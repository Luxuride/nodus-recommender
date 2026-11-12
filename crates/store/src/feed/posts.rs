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
    pub status_remote_id: String,
    pub account_id: i64,
    pub account_remote_id: Option<String>,
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
    pub account_instance_host: String,
    pub account_mastodon_id: i64,
    pub instance_host: String,
    pub url: Option<String>,
    pub media: Value,
    pub sensitive: bool,
    pub spoiler_text: String,
    pub emojis: Value,
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
            coalesce(s.remote_id, s.mastodon_id::text)                AS status_remote_id,
            s.account_id                                           AS account_id,
            af.affinity::double precision                          AS affinity,
            coalesce(s.content, '')                                AS content,
            s.favourites_count,
            s.reblogs_count,
            s.replies_count,
            s.created_at                                           AS published_at,
            s.in_reply_to_id IS NOT NULL                           AS is_reply,
            a.remote_id                                             AS account_remote_id,
            a.acct::text                                           AS account_acct,
            a.display_name                                         AS account_display_name,
            a.avatar_url                                           AS account_avatar_url,
            a.followers_count                                      AS account_followers_count,
            a.instance_host::text                                  AS instance_host,
            a.instance_host::text                                  AS account_instance_host,
            a.mastodon_id                                          AS account_mastodon_id,
            s.url,
            coalesce(s.media, '[]'::jsonb)                          AS media,
            s.sensitive,
            coalesce(s.spoiler_text, '')                           AS spoiler_text,
            coalesce(s.emojis, '[]'::jsonb)                         AS emojis
        FROM account_affinity af
        JOIN account a ON a.id = af.account_id
        JOIN status s ON s.account_id = af.account_id
        JOIN me ON true
        WHERE af.user_id = $1
          AND af.expires_at > now()
          AND s.visibility IN ('public', 'unlisted')
          AND btrim(coalesce(s.content, '')) <> ''
          AND s.expires_at > now()
          AND s.created_at > now() - ($3::int * interval '1 day')
          AND a.moved_to_account_id IS NULL
          AND a.discoverable IS DISTINCT FROM false
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

/// Posts to show when nobody has an affinity row yet.
///
/// A one-user instance has no peer to be similar to, so the personal pool is
/// legitimately empty.  The fallback keeps the feed useful: it takes the most
/// followed authors' recent posts, including authors the reader already follows.
/// That duplication of the home timeline is the honest price of a cold start.
pub async fn fallback_candidates(
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
            coalesce(s.remote_id, s.mastodon_id::text)                AS status_remote_id,
            s.account_id                                           AS account_id,
            0.5::double precision                                  AS affinity,
            coalesce(s.content, '')                                AS content,
            s.favourites_count,
            s.reblogs_count,
            s.replies_count,
            s.created_at                                           AS published_at,
            s.in_reply_to_id IS NOT NULL                           AS is_reply,
            a.remote_id                                             AS account_remote_id,
            a.acct::text                                           AS account_acct,
            a.display_name                                         AS account_display_name,
            a.avatar_url                                           AS account_avatar_url,
            a.followers_count                                      AS account_followers_count,
            a.instance_host::text                                  AS instance_host,
            a.instance_host::text                                  AS account_instance_host,
            a.mastodon_id                                          AS account_mastodon_id,
            s.url,
            coalesce(s.media, '[]'::jsonb)                          AS media,
            s.sensitive,
            coalesce(s.spoiler_text, '')                           AS spoiler_text,
            coalesce(s.emojis, '[]'::jsonb)                         AS emojis
        FROM status s
        JOIN account a ON a.id = s.account_id
        JOIN me ON true
        WHERE s.visibility IN ('public', 'unlisted')
          AND btrim(coalesce(s.content, '')) <> ''
          AND s.expires_at > now()
          AND s.created_at > now() - ($3::int * interval '1 day')
          AND a.moved_to_account_id IS NULL
          AND a.discoverable IS DISTINCT FROM false
          AND a.followers_count < 50000
          AND a.id <> me.account_id
          AND NOT EXISTS (
              SELECT 1
              FROM interaction_event e
              JOIN recommendation_item ri ON ri.id = e.item_id
              WHERE e.user_id = $1
                AND ri.account_id = a.id
                AND e.kind IN ('dismiss', 'not_interested')
          )
        ORDER BY a.followers_count DESC, s.created_at DESC
        LIMIT $2
        ",
    )
    .bind(user_id)
    .bind(limit)
    .bind(365_i32)
    .fetch_all(pool)
    .await
}

/// A scored post, ready to be minted as a `recommendation_item`.
///
/// The author is deliberately absent: `recommendation_item_target_matches_kind`
/// requires a `post` item to point at the status alone, and the author is one join
/// away through `status.account_id`.
#[derive(Debug)]
pub struct NewPostItem {
    pub status_id: i64,
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
    let scores = items.iter().map(|item| item.score).collect::<Vec<_>>();
    let reasons = items
        .iter()
        .map(|item| item.reason.clone())
        .collect::<Vec<_>>();

    sqlx::query_as::<_, (i64, i64)>(
        r"
        INSERT INTO recommendation_item
            (user_id, feed_id, kind, status_id, score, reason, model_version)
        SELECT $1, $2, 'post', s.status_id, s.score, s.reason, $3
        FROM UNNEST($4::bigint[], $5::float8[], $6::jsonb[])
            AS s(status_id, score, reason)
        RETURNING id, status_id
        ",
    )
    .bind(user_id)
    .bind(feed_id)
    .bind(model_version)
    .bind(&statuses)
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
    pub status_remote_id: String,
    pub account_acct: String,
    pub account_remote_id: Option<String>,
    pub account_display_name: Option<String>,
    pub account_avatar_url: Option<String>,
    pub account_instance_host: String,
    pub account_mastodon_id: i64,
    pub content: String,
    pub published_at: OffsetDateTime,
    pub favourites_count: i32,
    pub reblogs_count: i32,
    pub replies_count: i32,
    pub instance_host: String,
    pub url: Option<String>,
    pub media: Value,
    pub sensitive: bool,
    pub spoiler_text: String,
    pub emojis: Value,
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
            coalesce(s.remote_id, s.mastodon_id::text)               AS status_remote_id,
            a.remote_id                                             AS account_remote_id,
            a.acct::text                                           AS account_acct,
            a.display_name                                         AS account_display_name,
            a.avatar_url                                           AS account_avatar_url,
            coalesce(s.content, '')                                AS content,
            s.created_at                                           AS published_at,
            s.favourites_count,
            s.reblogs_count,
            s.replies_count,
            a.instance_host::text                                  AS instance_host,
            a.instance_host::text                                  AS account_instance_host,
            a.mastodon_id                                          AS account_mastodon_id,
            s.url,
            coalesce(s.media, '[]'::jsonb)                          AS media,
            s.sensitive,
            coalesce(s.spoiler_text, '')                           AS spoiler_text,
            coalesce(s.emojis, '[]'::jsonb)                         AS emojis
        FROM recommendation_item ri
        JOIN status s ON s.id = ri.status_id
        JOIN account a ON a.id = s.account_id
        WHERE ri.user_id = $1
          AND ri.feed_id = $2
          AND ri.kind = 'post'
          AND btrim(coalesce(s.content, '')) <> ''
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

/// Fresh posts from authors the user already has taste for.
///
/// The affinity query orders by strength before recency, so an author with a high
/// score can fill the pool with week-old posts while someone the user likes a little
/// less just posted. This rung asks the opposite question: what is new *right now*
/// from anyone the user has an affinity for?
pub async fn fresh_affinity_candidates(
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
            s.id AS status_id, coalesce(s.remote_id, s.mastodon_id::text) AS status_remote_id,
            s.account_id AS account_id,
            af.affinity::double precision AS affinity,
            coalesce(s.content, '') AS content,
            s.favourites_count, s.reblogs_count, s.replies_count,
            s.created_at AS published_at,
            s.in_reply_to_id IS NOT NULL AS is_reply,
            a.remote_id AS account_remote_id,
            a.acct::text AS account_acct, a.display_name AS account_display_name,
            a.avatar_url AS account_avatar_url, a.followers_count AS account_followers_count,
            a.instance_host::text AS instance_host,
            a.instance_host::text AS account_instance_host,
            a.mastodon_id AS account_mastodon_id, s.url,
            coalesce(s.media, '[]'::jsonb) AS media, s.sensitive,
            coalesce(s.spoiler_text, '') AS spoiler_text,
            coalesce(s.emojis, '[]'::jsonb) AS emojis
        FROM account_affinity af
        JOIN account a ON a.id = af.account_id
        JOIN status s ON s.account_id = af.account_id
        JOIN me ON true
        WHERE af.user_id = $1
          AND af.expires_at > now()
          AND s.visibility IN ('public', 'unlisted')
          AND btrim(coalesce(s.content, '')) <> ''
          AND s.expires_at > now()
          AND s.created_at > now() - interval '3 days'
          AND a.moved_to_account_id IS NULL
          AND a.discoverable IS DISTINCT FROM false
          AND NOT EXISTS (
              SELECT 1 FROM follow_edge fe
              WHERE fe.follower_id = me.account_id AND fe.followee_id = a.id
          )
          AND NOT EXISTS (
              SELECT 1 FROM interaction_event e
              JOIN recommendation_item ri ON ri.id = e.item_id
              WHERE e.user_id = $1 AND ri.account_id = a.id
                AND e.kind IN ('dismiss', 'not_interested')
          )
        ORDER BY s.created_at DESC, af.affinity DESC
        LIMIT $2
        ",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Public posts from the last week, regardless of affinity.
///
/// Freshness is its own signal: on a quiet instance the affinity snapshot can be
/// empty while the federated timeline is not. These rows give the ranker
/// something current to weigh, with a deliberately modest affinity so a known
/// author still wins when both are fresh.
pub async fn recent_candidates(
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
            s.id AS status_id, coalesce(s.remote_id, s.mastodon_id::text) AS status_remote_id,
            s.account_id AS account_id,
            0.25::double precision AS affinity,
            coalesce(s.content, '') AS content,
            s.favourites_count, s.reblogs_count, s.replies_count,
            s.created_at AS published_at,
            s.in_reply_to_id IS NOT NULL AS is_reply,
            a.remote_id AS account_remote_id,
            a.acct::text AS account_acct, a.display_name AS account_display_name,
            a.avatar_url AS account_avatar_url, a.followers_count AS account_followers_count,
            a.instance_host::text AS instance_host,
            a.instance_host::text AS account_instance_host,
            a.mastodon_id AS account_mastodon_id, s.url,
            coalesce(s.media, '[]'::jsonb) AS media, s.sensitive,
            coalesce(s.spoiler_text, '') AS spoiler_text,
            coalesce(s.emojis, '[]'::jsonb) AS emojis
        FROM status s
        JOIN account a ON a.id = s.account_id
        JOIN me ON true
        WHERE s.visibility IN ('public', 'unlisted')
          AND btrim(coalesce(s.content, '')) <> ''
          AND s.expires_at > now()
          AND s.created_at > now() - interval '7 days'
          AND a.moved_to_account_id IS NULL
          AND a.discoverable IS DISTINCT FROM false
          AND a.followers_count < 50000
          AND a.id <> me.account_id
          AND NOT EXISTS (
              SELECT 1 FROM follow_edge fe
              WHERE fe.follower_id = me.account_id AND fe.followee_id = a.id
          )
          AND NOT EXISTS (
              SELECT 1 FROM interaction_event e
              JOIN recommendation_item ri ON ri.id = e.item_id
              WHERE e.user_id = $1 AND ri.account_id = a.id
                AND e.kind IN ('dismiss', 'not_interested')
          )
        ORDER BY s.created_at DESC
        LIMIT $2
        ",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Posts from accounts two to four follows away.
///
/// This is the exploration rung. Affinity only knows people the user's neighbourhood
/// already agreed on; a friend of a friend is where a new interest comes from. The
/// graph is walked from the accounts the user follows, and the first (shortest) path
/// decides the weight: a two-hop account is a stronger guess than a four-hop one.
pub async fn exploration_candidates(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<PostCandidate>, sqlx::Error> {
    sqlx::query_as::<_, PostCandidate>(
        r"
        WITH RECURSIVE me AS (
            SELECT a.id AS account_id
            FROM app_user u
            JOIN account a
              ON a.instance_host = u.instance_host
             AND a.mastodon_id = u.mastodon_account_id
            WHERE u.id = $1
        ),
        seed(account_id) AS (
            SELECT fe.followee_id
            FROM me
            JOIN follow_edge fe ON fe.follower_id = me.account_id
            UNION
            SELECT pe.target_account_id
            FROM me
            JOIN profile_edge pe ON pe.account_id = me.account_id AND pe.kind = 'follow'
        ),
        reach(account_id, depth) AS (
            SELECT account_id, 1 FROM seed
            UNION
            SELECT next.account_id, reach.depth + 1
            FROM reach
            JOIN LATERAL (
                SELECT fe.followee_id AS account_id
                FROM follow_edge fe
                WHERE fe.follower_id = reach.account_id
                UNION
                SELECT pe.target_account_id
                FROM profile_edge pe
                WHERE pe.account_id = reach.account_id AND pe.kind = 'follow'
            ) next ON true
            WHERE reach.depth < 4
        ),
        hop AS (
            SELECT account_id, min(depth) AS depth
            FROM reach
            WHERE depth BETWEEN 2 AND 4
            GROUP BY account_id
        )
        SELECT
            s.id AS status_id, coalesce(s.remote_id, s.mastodon_id::text) AS status_remote_id,
            s.account_id AS account_id,
            GREATEST(0.16, 0.5 - 0.08 * hop.depth)::double precision AS affinity,
            coalesce(s.content, '') AS content,
            s.favourites_count, s.reblogs_count, s.replies_count,
            s.created_at AS published_at,
            s.in_reply_to_id IS NOT NULL AS is_reply,
            a.remote_id AS account_remote_id,
            a.acct::text AS account_acct, a.display_name AS account_display_name,
            a.avatar_url AS account_avatar_url, a.followers_count AS account_followers_count,
            a.instance_host::text AS instance_host,
            a.instance_host::text AS account_instance_host,
            a.mastodon_id AS account_mastodon_id, s.url,
            coalesce(s.media, '[]'::jsonb) AS media, s.sensitive,
            coalesce(s.spoiler_text, '') AS spoiler_text,
            coalesce(s.emojis, '[]'::jsonb) AS emojis
        FROM hop
        JOIN account a ON a.id = hop.account_id
        JOIN LATERAL (
            SELECT s.*
            FROM status s
            WHERE s.account_id = a.id
              AND s.visibility IN ('public', 'unlisted')
              AND btrim(coalesce(s.content, '')) <> ''
              AND s.expires_at > now()
              AND s.created_at > now() - ($3::int * interval '1 day')
            ORDER BY s.created_at DESC
            LIMIT 2
        ) s ON true
        JOIN me ON true
        WHERE a.moved_to_account_id IS NULL
          AND a.discoverable IS DISTINCT FROM false
          AND a.id <> me.account_id
          AND NOT EXISTS (
              SELECT 1 FROM follow_edge fe
              WHERE fe.follower_id = me.account_id AND fe.followee_id = a.id
          )
          AND NOT EXISTS (
              SELECT 1 FROM interaction_event e
              JOIN recommendation_item ri ON ri.id = e.item_id
              WHERE e.user_id = $1 AND ri.account_id = a.id
                AND e.kind IN ('dismiss', 'not_interested')
          )
        ORDER BY hop.depth, a.followers_count DESC, s.created_at DESC
        LIMIT $2
        ",
    )
    .bind(user_id)
    .bind(limit)
    .bind(14_i32)
    .fetch_all(pool)
    .await
}

/// What is getting attention right now, whoever wrote it.
///
/// Engagement per hour rather than raw engagement: a post with a hundred favourites
/// from last month is not trending, it is history. The affinity is neutral because
/// this rung is evidence about the post, not about the reader's neighbourhood.
pub async fn trending_candidates(
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
            s.id AS status_id, coalesce(s.remote_id, s.mastodon_id::text) AS status_remote_id,
            s.account_id AS account_id,
            0.3::double precision AS affinity,
            coalesce(s.content, '') AS content,
            s.favourites_count, s.reblogs_count, s.replies_count,
            s.created_at AS published_at,
            s.in_reply_to_id IS NOT NULL AS is_reply,
            a.remote_id AS account_remote_id,
            a.acct::text AS account_acct, a.display_name AS account_display_name,
            a.avatar_url AS account_avatar_url, a.followers_count AS account_followers_count,
            a.instance_host::text AS instance_host,
            a.instance_host::text AS account_instance_host,
            a.mastodon_id AS account_mastodon_id, s.url,
            coalesce(s.media, '[]'::jsonb) AS media, s.sensitive,
            coalesce(s.spoiler_text, '') AS spoiler_text,
            coalesce(s.emojis, '[]'::jsonb) AS emojis
        FROM status s
        JOIN account a ON a.id = s.account_id
        JOIN me ON true
        WHERE s.visibility IN ('public', 'unlisted')
          AND btrim(coalesce(s.content, '')) <> ''
          AND s.expires_at > now()
          AND s.created_at > now() - interval '3 days'
          AND s.favourites_count + 2 * s.reblogs_count > 0
          AND a.moved_to_account_id IS NULL
          AND a.discoverable IS DISTINCT FROM false
          AND a.followers_count < 50000
          AND a.id <> me.account_id
          AND NOT EXISTS (
              SELECT 1 FROM follow_edge fe
              WHERE fe.follower_id = me.account_id AND fe.followee_id = a.id
          )
          AND NOT EXISTS (
              SELECT 1 FROM interaction_event e
              JOIN recommendation_item ri ON ri.id = e.item_id
              WHERE e.user_id = $1 AND ri.account_id = a.id
                AND e.kind IN ('dismiss', 'not_interested')
          )
        ORDER BY
            (s.favourites_count + 2 * s.reblogs_count)::double precision
                / (extract(epoch FROM now() - s.created_at) / 3600.0 + 2.0) DESC,
            s.created_at DESC
        LIMIT $2
        ",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Accounts the reader's follows also follow, and authors the reader has already
/// boosted, replied to or followed. This is the feed's personal rung: it never
/// starts from the public timeline, so a large remote account cannot enter on
/// reach alone.
#[expect(
    clippy::too_many_lines,
    reason = "one readable statement of the personal candidate query beats four helpers"
)]
pub async fn personal_candidates(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<PostCandidate>, sqlx::Error> {
    sqlx::query_as::<_, PostCandidate>(
        r"
        WITH me AS (
            SELECT account.id AS account_id
            FROM app_user
            JOIN account
              ON account.instance_host = app_user.instance_host
             AND account.mastodon_id = app_user.mastodon_account_id
            WHERE app_user.id = $1
        ),
        followed AS (
            SELECT followee_id AS account_id
            FROM follow_edge
            WHERE follower_id = (SELECT account_id FROM me)
        ),
        shared AS (
            SELECT peer.followee_id AS account_id,
                   LEAST(0.90, 0.25 + 0.08 * count(*))::real AS affinity
            FROM followed AS mine
            JOIN follow_edge AS peer ON peer.follower_id = mine.account_id
            WHERE peer.followee_id <> (SELECT account_id FROM me)
              AND NOT EXISTS (
                  SELECT 1 FROM followed AS already
                  WHERE already.account_id = peer.followee_id
              )
            GROUP BY peer.followee_id
            HAVING count(*) >= 2
        ),
        acted AS (
            SELECT coalesce(item.account_id, status.account_id) AS account_id,
                   LEAST(0.95, 0.50 + 0.05 * count(*))::real AS affinity
            FROM interaction_event AS event
            JOIN recommendation_item AS item ON item.id = event.item_id
            LEFT JOIN status ON status.id = item.status_id
            WHERE event.user_id = $1
              AND event.kind IN ('boost', 'reply', 'follow', 'click')
            GROUP BY 1
        ),
        source AS (
            SELECT account_id, affinity FROM shared
            UNION ALL
            SELECT account_id, affinity FROM acted
        ),
        best AS (
            SELECT account_id, max(affinity) AS affinity
            FROM source
            GROUP BY account_id
        ),
        ranked AS (
            SELECT s.id AS status_id,
                   s.created_at,
                   s.content,
                   s.url,
                   coalesce(s.media, '[]'::jsonb) AS media,
                   s.sensitive,
                   coalesce(s.spoiler_text, '') AS spoiler_text,
                   coalesce(s.emojis, '[]'::jsonb) AS emojis,
                   a.id AS account_id,
                   a.instance_host,
                   a.acct,
                   coalesce(nullif(a.display_name, ''), a.acct) AS account_display_name,
                   coalesce(a.avatar_url, '') AS account_avatar_url,
                   s.favourites_count,
                   s.reblogs_count,
                   s.replies_count,
                   best.affinity,
                   row_number() OVER (
                       PARTITION BY a.id
                       ORDER BY s.created_at DESC
                   ) AS position
            FROM best
            JOIN account AS a ON a.id = best.account_id
            JOIN status AS s ON s.account_id = a.id
            WHERE s.visibility IN ('public', 'unlisted')
              AND btrim(coalesce(s.content, '')) <> ''
              AND s.expires_at > now()
              AND s.created_at > now() - interval '30 days'
              AND a.moved_to_account_id IS NULL
              AND a.discoverable IS DISTINCT FROM false
              AND NOT EXISTS (
                  SELECT 1 FROM interaction_event AS event
                  WHERE event.user_id = $1
                    AND event.item_id IN (
                        SELECT id FROM recommendation_item
                        WHERE status_id = s.id AND user_id = $1
                    )
                    AND event.kind IN ('dismiss', 'not_interested')
              )
        )
        SELECT status_id, created_at, content, url, media, sensitive, spoiler_text,
               emojis, account_id, instance_host, acct, account_display_name,
               account_avatar_url, favourites_count, reblogs_count, replies_count,
               affinity
        FROM ranked
        WHERE position <= 2
        ORDER BY affinity DESC, created_at DESC
        LIMIT $2
        ",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Text the reader has already signalled interest in: posts they boosted or
/// replied through the recommender, recent posts from accounts they follow, and
/// posts from authors they favourited or boosted.
/// The caller turns these into a multi-interest vector profile.
pub async fn interest_texts(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        r"
        WITH me AS (
            SELECT account.id AS account_id
            FROM app_user
            JOIN account
              ON account.instance_host = app_user.instance_host
             AND account.mastodon_id = app_user.mastodon_account_id
            WHERE app_user.id = $1
        ),
        acted AS (
            SELECT status.content
            FROM interaction_event AS event
            JOIN recommendation_item AS item ON item.id = event.item_id
            JOIN status ON status.id = item.status_id
            WHERE event.user_id = $1
              AND event.kind IN ('boost', 'reply', 'follow', 'click')
              AND btrim(coalesce(status.content, '')) <> ''
            ORDER BY event.server_ts DESC
            LIMIT $2
        ),
        followed AS (
            SELECT status.content
            FROM follow_edge AS edge
            JOIN status ON status.account_id = edge.followee_id
            WHERE edge.follower_id = (SELECT account_id FROM me)
              AND status.created_at > now() - interval '30 days'
              AND btrim(coalesce(status.content, '')) <> ''
            ORDER BY status.created_at DESC
            LIMIT $2
        ),
        favourited AS (
            SELECT status.content
            FROM engagement_edge AS edge
            JOIN status ON status.account_id = edge.target_id
            WHERE edge.actor_id = (SELECT account_id FROM me)
              AND edge.kind IN ('favourite', 'reblog')
              AND status.created_at > now() - interval '90 days'
              AND btrim(coalesce(status.content, '')) <> ''
            ORDER BY edge.last_seen_at DESC
            LIMIT $2
        )
        SELECT content FROM acted
        UNION ALL
        SELECT content FROM followed
        UNION ALL
        SELECT content FROM favourited
        LIMIT $2
        ",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}
