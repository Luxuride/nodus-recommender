//! The cold-start rung: instance-popular accounts, ranked without affinity.
//!
//! A user who has just connected has no `account_affinity` rows yet, because the
//! calculator has not seen their graphs. Serving an empty feed until it has is a bad
//! first impression, and serving a fabricated affinity would be dishonest. This
//! query is the honest middle: the same exclusions as
//! [`super::candidate::candidates`], ordered by how many followers each account has,
//! with the affinity column set to the neutral value so the ranking weights treat
//! every row alike.

use sqlx::PgPool;
use uuid::Uuid;

use super::candidate::Candidate;

/// Popular accounts on the instance, best first, with the feed's exclusions applied.
///
/// The exclusions are the same as [`super::candidates`] — followed, dismissed or not
/// interested, moved, not discoverable — but there is no `account_affinity` join and
/// no expiry to honour. Rejections stay a plain exclusion here on purpose: this rung
/// is served because no affinity has been materialised yet, so there is no evidence
/// either way about whether a rejection has softened. Until the personalised rung can
/// report decay, silence is safer than re-showing an account the user asked to stop
/// seeing. Order is `followers_count DESC, last_status_at DESC NULLS
/// LAST, account_id`: the follower count is the only signal, and the other two make
/// the order deterministic between rows that tie on it.
///
/// `affinity` is `1.0`, `support` is `0` and `sources` is empty, so a reader can tell
/// this row apart from a calculated one; [`super::candidate::Candidate::last_shown_at`],
/// `seen_count_7d`, `rejected_kind`, `rejected_at`, `impressions` and the
/// follow-through counters are empty because
/// this rung has no impression history to read.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn popular_candidates(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
) -> Result<Vec<Candidate>, sqlx::Error> {
    sqlx::query_as::<_, Candidate>(
        "WITH me AS ( \
             SELECT a.id AS account_id \
             FROM app_user u \
             JOIN account a ON a.instance_host = u.instance_host \
                 AND a.mastodon_id = u.mastodon_account_id \
             WHERE u.id = $1 \
         ) \
         SELECT a.id AS account_id, 1.0::float8 AS affinity, 0::smallint AS support, \
             ARRAY[]::text[] AS sources, now() AS computed_at, \
             NULL::timestamptz AS last_shown_at, 0::integer AS seen_count_7d, \
             NULL::text AS rejected_kind, NULL::timestamptz AS rejected_at, \
             0::integer AS impressions, \
             NULL::integer AS follow_through_shown, \
             NULL::integer AS follow_through_followed, \
             NULL::float8 AS expected_follow_rate, \
             a.acct::text AS acct, \
             a.display_name, a.note, a.avatar_url, a.followers_count, a.bot, \
             a.last_status_at, a.instance_host::text AS instance_host \
         FROM account a \
         LEFT JOIN me ON true \
         WHERE a.id IS DISTINCT FROM me.account_id \
             AND a.moved_to_account_id IS NULL \
             AND a.discoverable IS DISTINCT FROM false \
             AND NOT EXISTS ( \
                 SELECT 1 FROM follow_edge f \
                 WHERE f.follower_id = me.account_id AND f.followee_id = a.id \
             ) \
             AND NOT EXISTS ( \
                 SELECT 1 FROM interaction_event e \
                 JOIN recommendation_item ri ON ri.id = e.item_id \
                 WHERE e.user_id = $1 \
                     AND ri.account_id = a.id \
                     AND e.kind IN ('dismiss', 'not_interested') \
             ) \
         ORDER BY a.followers_count DESC, a.last_status_at DESC NULLS LAST, a.id \
         LIMIT $2",
    )
    .bind(user_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}
