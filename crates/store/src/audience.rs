//! Maintains `account.audience_weight`, the reach signal D24's refresh tiers use.
//!
//! The weight is the number of users whose affinity set contains an account:
//! literally "how many people could be shown this". D24 turns it into a refresh
//! interval (`hot` at 25 or more, `warm` at 1, `cold` otherwise) and into the
//! scheduler's ordering, so it has to live on `account` itself — the scheduler
//! pages one index on that table, and a `GROUP BY account_affinity` per candidate
//! cannot serve an index-ordered scan.
//!
//! The value is derived, not authoritative: [`recompute`] overwrites it after an
//! affinity pass, so it is stale by at most one recompute cycle. The scheduler
//! tolerates that on purpose — a stale count costs a fetch at the wrong tier, it
//! never changes what the feed is allowed to return.

use sqlx::PgPool;

/// What one pass changed, so the caller can log more than "done".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudienceRefresh {
    /// Accounts whose stored count was moved to the current affinity-set size.
    pub updated: u64,
    /// Accounts that left every affinity set and were reset to zero.
    pub cleared: u64,
}

/// Recompute `account.audience_weight` from `account_affinity`.
///
/// Two statements, because one `UPDATE ... FROM (SELECT ... GROUP BY ...)` cannot
/// see the accounts that have *no* affinity rows: they are absent from the grouped
/// side, so they would keep a stale positive count forever. The first statement
/// lifts the counts that have rows; the second zeroes the accounts that no longer
/// have any.
///
/// Both statements carry a `<>` guard, which is what makes a repeated pass cheap.
/// Without it every pass would rewrite every account row whose count did not
/// change — dead tuples, index churn, WAL and autovacuum work for no information
/// gain, on a pass that runs after every affinity recompute.
///
/// The statements touch disjoint row sets (the first only accounts with a grouped
/// count, which is at least one; the second only accounts without one), so they do
/// not need a transaction: an interleaved read sees a mix of old and new counts,
/// which is exactly the staleness this column already accepts.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn recompute(pool: &PgPool) -> Result<AudienceRefresh, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE account AS a \
         SET audience_weight = counts.weight \
         FROM ( \
             SELECT account_id, count(*)::integer AS weight \
             FROM account_affinity \
             GROUP BY account_id \
         ) AS counts \
         WHERE a.id = counts.account_id \
             AND a.audience_weight <> counts.weight",
    )
    .execute(pool)
    .await?
    .rows_affected();

    let cleared = sqlx::query(
        "UPDATE account \
         SET audience_weight = 0 \
         WHERE audience_weight <> 0 \
             AND NOT EXISTS ( \
                 SELECT 1 FROM account_affinity af \
                 WHERE af.account_id = account.id \
             )",
    )
    .execute(pool)
    .await?
    .rows_affected();

    Ok(AudienceRefresh { updated, cleared })
}
