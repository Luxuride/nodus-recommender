//! Writes for the precomputed candidate table.
//!
//! `account_affinity` is what makes a feed request a single indexed read
//! ([`crate::feed::candidates`]); the calculator materialises it after each
//! similarity pass. This module owns that write so the handler only deals in
//! already-scored rows.
//!
//! Replacement is atomic: the user's previous candidates are deleted and the new
//! top set inserted in one transaction. A recompute is idempotent, and a retry after
//! a lease timeout must not leave a half-written candidate set behind.

use sqlx::PgPool;
use uuid::Uuid;

/// Separator between generator names in the serialised `sources` column.
pub const SOURCE_SEPARATOR: &str = "|";

/// One candidate account and the score the similarity pass gave it.
#[derive(Debug, Clone, PartialEq)]
pub struct AffinityRow {
    /// Local account id.
    pub account_id: i64,
    /// Damped, normalised affinity.
    pub affinity: f32,
    /// Number of signals supporting this candidate.
    pub support: i16,
    /// Which generators contributed: `similar_user`, `two_hop` or both.
    pub sources: Vec<String>,
}

/// Replace one user's candidate accounts.
///
/// All previous rows for the user are removed, including rows another generator
/// might have written: a stale candidate is worse than a missing one. `sources`
/// records which generators contributed, so the feed can explain a recommendation
/// without recomputing the graph. The 7-day `expires_at` is set by the
/// insert so the feed never serves a snapshot older than the recompute cadence.
///
/// Returns the number of rows inserted.
///
/// # Errors
///
/// Propagates [`sqlx::Error`]. An account id that does not exist is a foreign-key
/// violation and indicates a bug upstream.
pub async fn replace_for_user(
    pool: &PgPool,
    user_id: Uuid,
    rows: &[AffinityRow],
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM account_affinity WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

    let inserted = if rows.is_empty() {
        0
    } else {
        let accounts: Vec<i64> = rows.iter().map(|row| row.account_id).collect();
        let affinities: Vec<f32> = rows.iter().map(|row| row.affinity).collect();
        let supports: Vec<i16> = rows.iter().map(|row| row.support).collect();
        // A nested array cannot be bound as `text[][]`; the sources are joined with a
        // separator that cannot occur in a generator name and split in SQL.
        let sources: Vec<String> = rows
            .iter()
            .map(|row| row.sources.join(SOURCE_SEPARATOR))
            .collect();

        sqlx::query(
            "INSERT INTO account_affinity \
                 (user_id, account_id, affinity, support, sources, expires_at) \
             SELECT $1, account_id, affinity, support, string_to_array(sources, '|'), \
                 now() + interval '7 days' \
             FROM UNNEST($2::bigint[], $3::real[], $4::smallint[], $5::text[]) \
                 AS page (account_id, affinity, support, sources)",
        )
        .bind(user_id)
        .bind(&accounts)
        .bind(&affinities)
        .bind(&supports)
        .bind(&sources)
        .execute(&mut *tx)
        .await?
        .rows_affected()
    };

    tx.commit().await?;
    Ok(inserted)
}
