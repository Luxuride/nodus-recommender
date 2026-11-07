//! Reference profiles: similarity against accounts we crawl, not only our users.
//!
//! The taste hop needs neighbours with a follow set. Our own users are the best
//! neighbours we have and the worst in supply — a young instance has three. Accounts
//! whose follows `expand_follows` walked are the alternative (D31): same comparison,
//! different source of `follow_edge` rows.
//!
//! What makes the comparison honest is the **coverage gate**. `expand_follows` stops
//! after a bounded number of pages, so a heavy follower's stored follows are a prefix
//! of the truth, and cosine over a prefix understates similarity in proportion to how
//! much is missing. Below [`MIN_COVERAGE`] of `account.following_count`, or with fewer
//! than [`MIN_KNOWN_FOLLOWS`] known follows, the account is not a reference: it is a
//! partial sample that would look dissimilar to everybody.
//!
//! Nothing here picks which accounts to crawl; that decision belongs to the expansion
//! pass. This module answers "given what we know, who resembles this user".

use sqlx::PgPool;
use uuid::Uuid;

/// Fraction of an account's follows that must be stored before it can be a reference.
///
/// At 0.8 the missing tail can still change a ranking, but the measured ordering is
/// stable enough to use; below it the cosine mostly measures crawl depth.
pub const MIN_COVERAGE: f64 = 0.8;

/// Lower bound on known follows, independent of coverage.
///
/// An account following twelve people has a ratio of 1.0 and nothing to compare.
pub const MIN_KNOWN_FOLLOWS: i64 = 40;

/// How many references one pass may compare against a single user.
///
/// The top fifty are kept; this is the candidate pool the scores come from.
pub const MAX_REFERENCES: i64 = 500;

/// An account that passed the coverage gate.
#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    /// The `account.id` of the candidate neighbour.
    pub account_id: i64,
    /// How many of its follows are stored.
    pub known_follows: i64,
    /// `known_follows / following_count`, in `0.0..=1.0`.
    pub coverage: f64,
}

/// Accounts that can serve as reference neighbours.
///
/// Users are excluded: they are compared through `user_similarity`, where favourites
/// and replies are available too, so scoring them here would double-count them at a
/// lower fidelity. Accounts without a published `following_count` are excluded as well,
/// because coverage is undefined and an unmeasurable neighbour is not evidence.
pub async fn gated(pool: &PgPool, limit: i64) -> Result<Vec<Reference>, sqlx::Error> {
    sqlx::query_as::<_, (i64, i64, f64)>(
        r"
        SELECT a.id,
               count(f.followee_id)::bigint AS known_follows,
               (count(f.followee_id)::float8
                    / NULLIF(a.following_count, 0)::float8) AS coverage
          FROM account a
          JOIN follow_edge f ON f.follower_id = a.id
         WHERE a.following_count > 0
           AND a.moved_to_account_id IS NULL
           AND NOT EXISTS (
                   SELECT 1
                     FROM app_user u
                    WHERE u.instance_host = a.instance_host
                      AND u.mastodon_account_id = a.mastodon_id
               )
         GROUP BY a.id, a.following_count
        HAVING count(f.followee_id) >= $1
           AND (count(f.followee_id)::float8
                    / NULLIF(a.following_count, 0)::float8) >= $2
         ORDER BY coverage DESC, count(f.followee_id) DESC, a.id
         LIMIT $3
        ",
    )
    .bind(MIN_KNOWN_FOLLOWS)
    .bind(MIN_COVERAGE)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|(account_id, known_follows, coverage)| Reference {
                account_id,
                known_follows,
                coverage,
            })
            .collect()
    })
}

/// The follows of one account, with the follower counts the IDF weight needs.
pub async fn follows_of(pool: &PgPool, account_id: i64) -> Result<Vec<(i64, i32)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, i32)>(
        r"
        SELECT f.followee_id, a.followers_count
          FROM follow_edge f
          JOIN account a ON a.id = f.followee_id
         WHERE f.follower_id = $1
        ",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await
}

/// One stored neighbour pair between a user and a reference account.
#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceRow {
    /// The user the row belongs to.
    pub user_id: Uuid,
    /// The reference account.
    pub account_id: i64,
    /// Blended similarity in `0.0..=1.0`.
    pub similarity: f32,
    /// The follow-set cosine, kept apart from the blend for explainability.
    pub co_follow: f32,
    /// Coverage at scoring time, so a surprising neighbour can be explained later.
    pub coverage: f32,
}

/// Replace a user's reference neighbours in one transaction.
pub async fn replace_for_user(
    pool: &PgPool,
    user_id: Uuid,
    rows: &[ReferenceRow],
) -> Result<u64, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("DELETE FROM reference_similarity WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *transaction)
        .await?;

    if !rows.is_empty() {
        let account_ids: Vec<i64> = rows.iter().map(|row| row.account_id).collect();
        let similarities: Vec<f32> = rows.iter().map(|row| row.similarity).collect();
        let co_follows: Vec<f32> = rows.iter().map(|row| row.co_follow).collect();
        let coverages: Vec<f32> = rows.iter().map(|row| row.coverage).collect();

        sqlx::query(
            r"
            INSERT INTO reference_similarity
                   (user_id, account_id, similarity, co_follow, coverage, computed_at)
            SELECT $1, account_id, similarity, co_follow, coverage, now()
              FROM UNNEST($2::bigint[], $3::real[], $4::real[], $5::real[])
                   AS t (account_id, similarity, co_follow, coverage)
            ",
        )
        .bind(user_id)
        .bind(&account_ids)
        .bind(&similarities)
        .bind(&co_follows)
        .bind(&coverages)
        .execute(&mut *transaction)
        .await?;
    }

    transaction.commit().await?;
    Ok(u64::try_from(rows.len()).unwrap_or(u64::MAX))
}

/// The neighbours stored for a user, strongest first.
pub async fn for_user(pool: &PgPool, user_id: Uuid) -> Result<Vec<ReferenceRow>, sqlx::Error> {
    sqlx::query_as::<_, (i64, f32, f32, f32)>(
        r"
        SELECT account_id, similarity, co_follow, coverage
          FROM reference_similarity
         WHERE user_id = $1
         ORDER BY similarity DESC, account_id
        ",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(
                |(account_id, similarity, co_follow, coverage)| ReferenceRow {
                    user_id,
                    account_id,
                    similarity,
                    co_follow,
                    coverage,
                },
            )
            .collect()
    })
}
