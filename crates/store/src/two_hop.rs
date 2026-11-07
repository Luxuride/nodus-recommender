//! The graph hop: accounts followed by people the reader follows.
//!
//! This is the second candidate source and the cheap one — it needs no similarity
//! pass, only the follow edges we already store. It is deliberately damped, because
//! "people you follow follow them" is closer to popularity than to taste.
//!
//! The damping is not a constant, though. If the reader has engaged with the bridge
//! account — favourited, boosted or replied to it — then that bridge is evidence about
//! *their* taste, not merely about who is popular in their neighbourhood, and the path
//! through it is promoted most of the way back to full weight (D35). That is what makes
//! depth adaptive: engaging with a second-hop account turns its follows into
//! first-class candidates without any global "expand everything two hops" cost.

use sqlx::PgPool;
use uuid::Uuid;

/// Damping for a bridge the reader has not engaged with.
pub const BRIDGE_WEIGHT: f64 = 0.25;

/// Damping for a bridge the reader has engaged with.
pub const ENGAGED_BRIDGE_WEIGHT: f64 = 0.6;

/// One candidate reached through the graph hop.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct BridgeRow {
    /// Local account id of the candidate.
    pub account_id: i64,
    /// Summed path weight: `w(bridge) · damping · w(candidate)` over every bridge.
    pub weight: f64,
    /// How many of the reader's followees follow this account.
    pub support: i16,
    /// How many of those bridges the reader has engaged with.
    pub engaged_paths: i16,
}

/// Candidate accounts reached through the people `user_id` follows.
///
/// Accounts the reader already follows, and the reader's own account, are excluded:
/// a candidate that is already followed cannot be recommended, and its presence would
/// inflate `support` for everything reachable through it.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn candidates(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<BridgeRow>, sqlx::Error> {
    sqlx::query_as::<_, BridgeRow>(
        "WITH me AS ( \
             SELECT a.id AS account_id \
             FROM app_user u \
             JOIN account a ON a.instance_host = u.instance_host \
                 AND a.mastodon_id = u.mastodon_account_id \
             WHERE u.id = $1 \
         ), \
         bridges AS ( \
             SELECT f2.followee_id AS account_id, \
                 f2.follower_id AS via, \
                 EXISTS ( \
                     SELECT 1 FROM engagement_edge e \
                     WHERE e.actor_id = me.account_id AND e.target_id = f2.follower_id \
                 ) AS engaged, \
                 1.0 / ln(exp(1) + bridge.followers_count) AS via_weight \
             FROM follow_edge f1 \
             JOIN me ON f1.follower_id = me.account_id \
             JOIN follow_edge f2 ON f2.follower_id = f1.followee_id \
             JOIN account bridge ON bridge.id = f2.follower_id \
             WHERE f2.followee_id <> me.account_id \
                 AND NOT EXISTS ( \
                     SELECT 1 FROM follow_edge f3 \
                     WHERE f3.follower_id = me.account_id AND f3.followee_id = f2.followee_id \
                 ) \
         ) \
         SELECT b.account_id, \
             SUM( \
                 b.via_weight * (1.0 / ln(exp(1) + a.followers_count)) \
                 * CASE WHEN b.engaged THEN $3::float8 ELSE $4::float8 END \
             )::float8 AS weight, \
             count(DISTINCT b.via)::smallint AS support, \
             count(DISTINCT b.via) FILTER (WHERE b.engaged)::smallint AS engaged_paths \
         FROM bridges b \
         JOIN account a ON a.id = b.account_id \
         WHERE a.moved_to_account_id IS NULL \
         GROUP BY b.account_id \
         ORDER BY weight DESC, b.account_id \
         LIMIT $2",
    )
    .bind(user_id)
    .bind(limit)
    .bind(ENGAGED_BRIDGE_WEIGHT)
    .bind(BRIDGE_WEIGHT)
    .fetch_all(pool)
    .await
}
