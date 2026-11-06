//! Reads and writes for the user–user similarity graph.
//!
//! The calculator's `recompute_similarity` handler is the only producer of
//! `user_similarity`; this module owns the SQL it needs so the handler stays
//! business logic. Three shapes are deliberate:
//!
//! * **A user's graph is loaded once per comparison.** [`graph`] returns the follow
//!   set both as plain account ids (for set difference) and as IDF-weighted
//!   [`UserSignals`] (for the core cosine math), plus the weight of every followed
//!   account so candidate propagation needs no second lookup.
//! * **Neighbours come from one self-join.** [`co_followers`] finds users who share
//!   at least one follow, ordered by how many follows they share, so the bounded
//!   candidate set is the *most* overlapping users rather than an arbitrary slice.
//! * **Replacement is atomic and idempotent.** [`replace_neighbours`] deletes and
//!   re-inserts in one transaction, which is what makes a recompute safe to retry
//!   after a lease timeout.
//!
//! The deployed `user_similarity` table has no `expires_at` column (see
//! `migrations/0001_initial.sql`); freshness comes from replacing the rows, not
//! from a TTL.

use std::collections::BTreeMap;

use nodus_core::idf;
use nodus_core::similarity::UserSignals;
use sqlx::PgPool;
use uuid::Uuid;

/// One stored neighbour row, in the shape the core comparison produces.
#[derive(Debug, Clone, PartialEq)]
pub struct NeighbourRow {
    /// The user whose top neighbours these are.
    pub user_id: Uuid,
    /// A similar user.
    pub similar_user_id: Uuid,
    /// Blended similarity in `[0, 1]`.
    pub similarity: f32,
    /// Co-follow component.
    pub co_follow: f32,
    /// Co-favourite component.
    pub co_fav: f32,
    /// Co-reply component.
    pub co_reply: f32,
    /// Reciprocity component.
    pub reciprocity: f32,
}

/// One user's signal sets, ready to compare.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserGraph {
    /// Followed accounts as local ids, for `F(v) \ F(u)`.
    pub follows: Vec<i64>,
    /// The IDF-weighted sets the core comparison consumes.
    pub signals: UserSignals,
    /// `follow_weight` for every id in [`Self::follows`], so candidate propagation
    /// can score an account without asking the database for its follower count again.
    pub follow_weights: BTreeMap<i64, f64>,
}

/// Load one user's follow and engagement graph.
///
/// Returns `None` when the user does not exist, is soft-deleted, or has no
/// `account` row yet — in all three cases there is nothing to compare, and the
/// caller must decide whether that is permanent (a named subject) or skippable
/// (a maintenance pass).
///
/// Follow weights use [`idf::follow_weight`] on the account's last known follower
/// count. Engagement rows are keyed by account id: favourites and boosts feed the
/// co-favourite set, replies feed the co-reply set. Engagement rows carry our own
/// per-actor count, not the global favourite count or the target's distinct engager
/// count that `idf::status_weight` / `idf::target_weight` expect, so applying those
/// weights here would invert the intended signal; entries are inserted at weight
/// `1.0` until the global counts are available.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn graph(pool: &PgPool, user_id: Uuid) -> Result<Option<UserGraph>, sqlx::Error> {
    let account_id = sqlx::query_scalar::<_, i64>(
        "SELECT a.id \
         FROM app_user u \
         JOIN account a ON a.instance_host = u.instance_host \
             AND a.mastodon_id = u.mastodon_account_id \
         WHERE u.id = $1 AND u.deleted_at IS NULL",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    let Some(account_id) = account_id else {
        return Ok(None);
    };

    let followees = sqlx::query_as::<_, (i64, i32)>(
        "SELECT f.followee_id, a.followers_count \
         FROM follow_edge f \
         JOIN account a ON a.id = f.followee_id \
         WHERE f.follower_id = $1",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;

    let mut graph = UserGraph {
        follows: Vec::with_capacity(followees.len()),
        signals: UserSignals::default(),
        follow_weights: BTreeMap::new(),
    };
    for (followee_id, followers_count) in followees {
        let count = followers_count.max(0).unsigned_abs();
        let weight = idf::follow_weight(count);
        graph.follows.push(followee_id);
        graph.follow_weights.insert(followee_id, weight);
        graph
            .signals
            .follows
            .insert(u64::try_from(followee_id).unwrap_or_default(), weight);
    }
    graph.follows.sort_unstable();

    let engagements = sqlx::query_as::<_, (i64, String, i32)>(
        "SELECT target_id, kind, count FROM engagement_edge WHERE actor_id = $1",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    for (target_id, kind, _count) in engagements {
        let id = u64::try_from(target_id).unwrap_or_default();
        // `count` is this actor's own engagement count. The documented IDF term is
        // the target's global popularity, which `engagement_edge` does not store, so
        // the signal is unweighted rather than weighted by the wrong number.
        match kind.as_str() {
            "favourite" | "reblog" => graph.signals.favourites.insert(id, 1.0),
            "reply" => graph.signals.replies.insert(id, 1.0),
            // The schema has a CHECK constraint on kind; an unknown value here would
            // be a schema we do not understand, and skipping it is safer than
            // inventing a signal.
            _ => {}
        }
    }

    Ok(Some(graph))
}

/// Users who share at least one follow with `user_id`, most overlaps first.
///
/// One self-join on `followee_id` does the counting: `f2` is every follow of a
/// candidate user, and grouping by that user keeps only the shared followees in
/// the count. Deleted users are excluded, and the caller is never returned as its
/// own neighbour.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn co_followers(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "WITH me AS ( \
             SELECT a.id AS account_id \
             FROM app_user u \
             JOIN account a ON a.instance_host = u.instance_host \
                 AND a.mastodon_id = u.mastodon_account_id \
             WHERE u.id = $1 AND u.deleted_at IS NULL \
         ) \
         SELECT u2.id \
         FROM me \
         JOIN follow_edge f1 ON f1.follower_id = me.account_id \
         JOIN follow_edge f2 ON f2.followee_id = f1.followee_id \
         JOIN account a2 ON a2.id = f2.follower_id \
         JOIN app_user u2 ON u2.instance_host = a2.instance_host \
             AND u2.mastodon_account_id = a2.mastodon_id \
         WHERE u2.id <> $1 AND u2.deleted_at IS NULL \
         GROUP BY u2.id \
         ORDER BY count(DISTINCT f1.followee_id) DESC, u2.id \
         LIMIT $2",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Replace one user's stored neighbours.
///
/// The delete makes an unfollow visible: a similarity graph that only ever grew
/// would keep propagating through ties the user has already dropped. Both
/// statements share a transaction, so a failed insert cannot leave the user with no
/// neighbours at all.
///
/// Returns the number of rows inserted (`0` for an empty slice, which still clears
/// the previous snapshot).
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn replace_neighbours(
    pool: &PgPool,
    user_id: Uuid,
    rows: &[NeighbourRow],
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM user_similarity WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

    let inserted = if rows.is_empty() {
        0
    } else {
        let similar: Vec<Uuid> = rows.iter().map(|row| row.similar_user_id).collect();
        let similarity: Vec<f32> = rows.iter().map(|row| row.similarity).collect();
        let co_follow: Vec<f32> = rows.iter().map(|row| row.co_follow).collect();
        let co_fav: Vec<f32> = rows.iter().map(|row| row.co_fav).collect();
        let co_reply: Vec<f32> = rows.iter().map(|row| row.co_reply).collect();
        let reciprocity: Vec<f32> = rows.iter().map(|row| row.reciprocity).collect();

        sqlx::query(
            "INSERT INTO user_similarity \
                 (user_id, other_user_id, similarity, co_follow, co_fav, co_reply, reciprocity) \
             SELECT $1, other_user_id, similarity, co_follow, co_fav, co_reply, reciprocity \
             FROM UNNEST($2::uuid[], $3::real[], $4::real[], $5::real[], $6::real[], $7::real[]) \
                 AS page (other_user_id, similarity, co_follow, co_fav, co_reply, reciprocity)",
        )
        .bind(user_id)
        .bind(&similar)
        .bind(&similarity)
        .bind(&co_follow)
        .bind(&co_fav)
        .bind(&co_reply)
        .bind(&reciprocity)
        .execute(&mut *tx)
        .await?
        .rows_affected()
    };

    tx.commit().await?;
    Ok(inserted)
}

/// Users whose own follow edges changed recently, for the maintenance pass.
///
/// A subject-less `recompute_similarity` job has no user to work on, so it sweeps
/// the users a recent sync has touched. The window is deliberately one day and the
/// list is bounded: the nightly full pass is a separate scheduling concern, and a
/// handler must fit inside one lease.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn recently_changed_users(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar::<_, Uuid>(
        "SELECT u.id \
         FROM app_user u \
         JOIN account a ON a.instance_host = u.instance_host \
             AND a.mastodon_id = u.mastodon_account_id \
         JOIN follow_edge f ON f.follower_id = a.id \
         WHERE u.deleted_at IS NULL \
             AND f.last_seen_at > now() - interval '1 day' \
         GROUP BY u.id \
         ORDER BY max(f.last_seen_at) DESC \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}
