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
    /// Attention spent on this account in the last 7 days, in units of the user's
    /// own median dwell.
    ///
    /// The fatigue term, and the reason a user who has scrolled past an account four
    /// times without tapping it stops seeing it: exposure without interaction is a
    /// negative signal, not a neutral one. It counts attention rather than page loads
    /// (D44), so a card skimmed past in 300 ms is worth a fraction of one that was
    /// read for the user's usual minute, and one never measured counts as a single
    /// plain impression.
    pub seen_attention_7d: f64,
    /// Kind of the most recent rejection (`dismiss` or `not_interested`), if any.
    ///
    /// Reported instead of filtered so the ranker can decay it (D26): an account
    /// excluded forever can never be recovered by the positive evidence that
    /// follows a changed mind.
    pub rejected_kind: Option<String>,
    /// When that rejection was recorded.
    pub rejected_at: Option<OffsetDateTime>,
    /// How many impressions this user has had of this account, all time.
    ///
    /// The shrinkage denominator for the rejection: one impression is thin
    /// evidence, two hundred are not, so the same rejection means less to a user
    /// who has barely seen the account.
    pub impressions: i32,
    /// Times this account was shown instance-wide in the evidence window.
    ///
    /// `None` when no counters exist yet: an account nobody has been shown has no
    /// follow-through, and the ranker must not read that as "shown, never
    /// followed" (D29).
    pub follow_through_shown: Option<i32>,
    /// Times a user followed it after one of those impressions.
    pub follow_through_followed: Option<i32>,
    /// The instance-wide baseline stored with the counters (D29).
    pub expected_follow_rate: Option<f64>,
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
/// someone you follow is not a recommendation), expired affinity, and accounts
/// whose owner said they are not discoverable. A rejection is deliberately not an
/// exclusion: the most recent `dismiss` or `not_interested` and the user's
/// impression count travel with the row so the ranker can turn them into a
/// decaying penalty (D26).
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn candidates(
    pool: &PgPool,
    user_id: Uuid,
    limit: i64,
    baseline_ms: Option<f64>,
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
             impressions.seen AS seen_attention_7d, \
             rej.kind AS rejected_kind, rej.server_ts AS rejected_at, \
             total_impressions.seen AS impressions, \
             ft.impressions AS follow_through_shown, \
             ft.follows AS follow_through_followed, \
             ft.expected_rate AS expected_follow_rate, \
             a.acct::text AS acct, \
             a.display_name, a.note, a.avatar_url, a.followers_count, a.bot, \
             a.last_status_at, a.instance_host::text AS instance_host \
         FROM account_affinity af \
         JOIN account a ON a.id = af.account_id \
         LEFT JOIN account_follow_through ft ON ft.account_id = af.account_id \
         LEFT JOIN me ON true \
         LEFT JOIN LATERAL ( \
             SELECT ri.served_at FROM recommendation_item ri \
             WHERE ri.user_id = $1 AND ri.account_id = af.account_id \
             ORDER BY ri.served_at DESC LIMIT 1 \
         ) AS shown ON true \
         LEFT JOIN LATERAL ( \
             SELECT COALESCE( \
                 SUM( \
                     CASE \
                         WHEN e.dwell_ms IS NULL THEN 1.0 \
                         WHEN $3::float8 IS NULL OR $3::float8 <= 0 THEN 1.0 \
                         ELSE LEAST(e.dwell_ms::float8 / $3::float8, 3.0) \
                     END \
                 ), \
                 0.0 \
             )::float8 AS seen \
             FROM interaction_event e \
             JOIN recommendation_item ri ON ri.id = e.item_id \
             WHERE e.user_id = $1 AND ri.account_id = af.account_id \
                 AND e.kind = 'impression' \
                 AND e.server_ts > now() - interval '7 days' \
         ) AS impressions ON true \
         LEFT JOIN LATERAL ( \
             SELECT e.kind, e.server_ts \
             FROM interaction_event e \
             JOIN recommendation_item ri ON ri.id = e.item_id \
             WHERE e.user_id = $1 AND ri.account_id = af.account_id \
                 AND e.kind IN ('dismiss', 'not_interested') \
             ORDER BY e.server_ts DESC LIMIT 1 \
         ) AS rej ON true \
         LEFT JOIN LATERAL ( \
             SELECT count(*)::integer AS seen \
             FROM interaction_event e \
             JOIN recommendation_item ri ON ri.id = e.item_id \
             WHERE e.user_id = $1 AND ri.account_id = af.account_id \
                 AND e.kind = 'impression' \
         ) AS total_impressions ON true \
         WHERE af.user_id = $1 \
             AND af.expires_at > now() \
             AND af.account_id IS DISTINCT FROM me.account_id \
             AND a.moved_to_account_id IS NULL \
             AND a.discoverable IS DISTINCT FROM false \
             AND NOT EXISTS ( \
                 SELECT 1 FROM follow_edge f \
                 WHERE f.follower_id = me.account_id AND f.followee_id = af.account_id \
             ) \
         ORDER BY af.affinity DESC, af.account_id \
         LIMIT $2",
    )
    .bind(user_id)
    .bind(limit)
    .bind(baseline_ms)
    .fetch_all(pool)
    .await
}
