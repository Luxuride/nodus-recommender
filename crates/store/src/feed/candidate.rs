//! The candidate row the feed ranks, and the affinity query that produces it.

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
             AND af.account_id IS DISTINCT FROM me.account_id \
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
