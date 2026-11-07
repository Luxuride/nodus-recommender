//! How often a recommended account was shown, and how often it was followed.
//!
//! This is the numerator and the denominator of the lift in D29: an account that is
//! followed more often than the average impression is evidence the ranking was
//! right, and one that is never followed is evidence it was wrong — but only when
//! it was actually shown. Nothing here is stored in `account`: the counters belong
//! to a recommendation, not to an account, and they expire with the evidence window.
//!
//! The baseline is stored with each row rather than recomputed per request. It is a
//! property of the whole instance, so a reader that had to derive it would either
//! scan the event table on every feed request or invent a different denominator —
//! and two denominators mean two different ladders.

use sqlx::PgPool;

/// Evidence window. Matches `EVENT_RETENTION_DAYS`: counting what retention is
/// about to delete would make the baseline jump on every purge.
const WINDOW_DAYS: i32 = 90;

/// One account's follow-through, as the feed reads it.
#[derive(Debug, Clone, Copy, sqlx::FromRow)]
pub struct FollowThrough {
    /// Times the account appeared in a delivered feed page.
    pub impressions: i32,
    /// Times a user followed it from one of those impressions.
    pub follows: i32,
    /// How many distinct users produced the impressions.
    pub distinct_users: i32,
    /// The instance-wide baseline at compute time, if any evidence existed.
    pub expected_rate: Option<f64>,
}

impl FollowThrough {
    /// Follows per impression, or `None` when the account was never shown.
    ///
    /// `None` rather than zero: an unshown account has no rate, and treating that
    /// as "followed by nobody" would punish every new candidate.
    #[must_use]
    pub fn observed_rate(&self) -> Option<f64> {
        if self.impressions <= 0 {
            return None;
        }
        Some(f64::from(self.follows) / f64::from(self.impressions))
    }
}

/// What one recompute changed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FollowThroughReport {
    /// Accounts whose counters were written or refreshed.
    pub accounts: u64,
    /// Rows removed because their evidence aged out.
    pub pruned: u64,
    /// The baseline those rows were written with.
    pub expected_rate: Option<f64>,
}

/// Recomputes every account's counters from the event log.
///
/// The window is fixed rather than per-account: a feed item from two months ago is
/// no more evidence than one from yesterday, and a per-account window would let a
/// single old follow keep an account's row alive forever.
pub async fn recompute(pool: &PgPool) -> Result<FollowThroughReport, sqlx::Error> {
    let expected_rate = expected_rate(pool).await?;

    let accounts = sqlx::query(
        r"
        INSERT INTO account_follow_through
            (account_id, impressions, follows, distinct_users, expected_rate, computed_at)
        SELECT
            ri.account_id,
            count(*) FILTER (WHERE e.kind = 'impression')::int,
            count(*) FILTER (WHERE e.kind = 'follow')::int,
            count(DISTINCT e.user_id)::int,
            $2,
            now()
        FROM interaction_event e
        JOIN recommendation_item ri ON ri.id = e.item_id
        WHERE ri.kind = 'account'
          AND e.kind IN ('impression', 'follow')
          AND e.server_ts > now() - ($1::int * interval '1 day')
        GROUP BY ri.account_id
        ON CONFLICT (account_id) DO UPDATE SET
            impressions    = EXCLUDED.impressions,
            follows        = EXCLUDED.follows,
            distinct_users = EXCLUDED.distinct_users,
            expected_rate  = EXCLUDED.expected_rate,
            computed_at    = EXCLUDED.computed_at
        ",
    )
    .bind(WINDOW_DAYS)
    .bind(expected_rate)
    .execute(pool)
    .await?
    .rows_affected();

    let pruned = sqlx::query(
        r"
        DELETE FROM account_follow_through ft
        WHERE NOT EXISTS (
            SELECT 1
            FROM interaction_event e
            JOIN recommendation_item ri ON ri.id = e.item_id
            WHERE ri.account_id = ft.account_id
              AND ri.kind = 'account'
              AND e.kind IN ('impression', 'follow')
              AND e.server_ts > now() - ($1::int * interval '1 day')
        )
        ",
    )
    .bind(WINDOW_DAYS)
    .execute(pool)
    .await?
    .rows_affected();

    Ok(FollowThroughReport {
        accounts,
        pruned,
        expected_rate,
    })
}

/// Follows per impression across every account in the window.
///
/// `None` while nothing has been shown, so a reader can wait for evidence instead of
/// dividing by a zero it invented.
pub async fn expected_rate(pool: &PgPool) -> Result<Option<f64>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<f64>>(
        r"
        SELECT CASE
                   WHEN count(*) = 0 THEN NULL
                   ELSE count(*) FILTER (WHERE e.kind = 'follow')::float8
                        / count(*)::float8
               END
        FROM interaction_event e
        JOIN recommendation_item ri ON ri.id = e.item_id
        WHERE ri.kind = 'account'
          AND e.kind IN ('impression', 'follow')
          AND e.server_ts > now() - ($1::int * interval '1 day')
        ",
    )
    .bind(WINDOW_DAYS)
    .fetch_one(pool)
    .await
}

/// Reads one account's counters, or `None` when it has no evidence yet.
pub async fn load(pool: &PgPool, account_id: i64) -> Result<Option<FollowThrough>, sqlx::Error> {
    sqlx::query_as::<_, FollowThrough>(
        r"
        SELECT impressions, follows, distinct_users, expected_rate
        FROM account_follow_through
        WHERE account_id = $1
        ",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await
}
