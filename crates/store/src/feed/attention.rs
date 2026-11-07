//! How much attention a user normally gives a card, so fatigue can weigh it.
//!
//! The fatigue term asks "has this person seen enough of this account?", and the
//! honest denominator is the person's own pace: one reader spends four seconds on a
//! post that another skims in 400 ms, and the same 400 ms means opposite things for
//! the two of them. The median of a user's own measured impressions is that pace —
//! a mean would be dragged by the tab left open over lunch, which is exactly the
//! behaviour the measurement is trying to discount.
//!
//! Below [`MIN_BASELINE_SAMPLES`] measured impressions there is no baseline: a median
//! of five samples is one scrolling session, not a habit. In that case fatigue falls
//! back to counting impressions one apiece, which is the behaviour that shipped before
//! attention was measured at all.

use sqlx::PgPool;
use uuid::Uuid;

/// Measured impressions needed before a median means anything.
pub const MIN_BASELINE_SAMPLES: i64 = 20;

/// How far back the baseline looks.
///
/// Long enough to survive a quiet week, short enough that a change in reading pace
/// (a commute, a new job) eventually shows up.
pub const BASELINE_WINDOW_DAYS: i32 = 30;

/// The user's median dwell across measured impressions, in milliseconds.
///
/// `None` when the user has too few measured impressions, has not consented to the
/// measurement, or dismisses every card faster than the clock can see.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn baseline_ms(pool: &PgPool, user_id: Uuid) -> Result<Option<f64>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<f64>>(
        "SELECT CASE \
             WHEN count(*) >= $2 \
             THEN percentile_cont(0.5) WITHIN GROUP (ORDER BY e.dwell_ms::float8) \
         END \
         FROM interaction_event e \
         WHERE e.user_id = $1 \
             AND e.kind = 'impression' \
             AND e.dwell_ms IS NOT NULL \
             AND e.server_ts > now() - ($3::int * interval '1 day')",
    )
    .bind(user_id)
    .bind(MIN_BASELINE_SAMPLES)
    .bind(BASELINE_WINDOW_DAYS)
    .fetch_one(pool)
    .await
}
