//! `recompute_similarity` — P3, a user's top-50 neighbours and their candidates.
//!
//! This is the producer the feed was missing: `store::feed::candidates` reads
//! `account_affinity`, and until this handler runs nothing writes it. The pass is
//! entirely local — no instance traffic — so it can retry cheaply.
//!
//! The flow, per user:
//!
//! 1. Load the caller's graph ([`similarity::graph`]) and the users who share at
//!    least one follow ([`similarity::co_followers`], bounded at 200).
//! 2. Compare against each neighbour with [`nodus_core::similarity::compare`] and
//!    blend the components into one score; keep the pairs at or above
//!    [`MIN_SIMILARITY`], sort descending and keep the top [`MAX_SIMILAR_USERS`].
//! 3. Replace the user's `user_similarity` rows.
//! 4. Propagate the neighbours' follows into `account_affinity`:
//!    `score[c] += similarity · follow_weight(c)`, with `support` counting the
//!    neighbours that follow `c`, keeping the top [`CANDIDATE_LIMIT`].
//!
//! A subject-less job is a maintenance pass over users whose follow edges changed in
//! the last day. A named subject that no longer has an account row is a permanent
//! failure; a user with no overlapping peers is normal and succeeds with an empty
//! snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use nodus_core::similarity::{
    CANDIDATE_LIMIT, MAX_SIMILAR_USERS, MIN_SIMILARITY, SimilarityBreakdown, SimilarityWeights,
    blend, compare,
};
use nodus_store::affinity::{self, AffinityRow};
use nodus_store::models::Job;
use nodus_store::similarity::{self, NeighbourRow, UserGraph};
use sqlx::PgPool;
use tracing::{debug, info};
use uuid::Uuid;

use super::{Handler, JobError};

/// How many overlapping users are compared per recompute.
///
/// The graph can contain thousands of peers; similarity is pruned afterwards, so
/// this is the recall/CPU trade-off. Two hundred is several times the 50 stored,
/// which leaves room for the top set to be stable when a few peers change.
const NEIGHBOUR_LIMIT: i64 = 200;

/// How many recently changed users one maintenance pass walks.
///
/// Bounded so the handler fits inside a lease; the nightly full pass is a separate
/// scheduling concern.
const MAINTENANCE_LIMIT: i64 = 500;

/// One compared neighbour, kept until the top set has been selected.
#[derive(Debug)]
struct ScoredNeighbour {
    user_id: Uuid,
    graph: UserGraph,
    breakdown: SimilarityBreakdown,
    similarity: f64,
}

/// The P3 handler.
#[derive(Debug)]
pub struct RecomputeSimilarity {
    pool: PgPool,
}

impl RecomputeSimilarity {
    /// A handler bound to one pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Recompute one user's neighbours and candidates.
    ///
    /// Returns `false` when the user has no `account` row (never synced, or
    /// soft-deleted). The caller decides whether that is permanent or skippable.
    ///
    /// # Errors
    ///
    /// A database failure becomes [`JobError::Retry`] with a 30 s hint: the work is
    /// idempotent, so retrying is always safe.
    async fn recompute_one(&self, user_id: Uuid) -> Result<bool, JobError> {
        let Some(graph) = similarity::graph(&self.pool, user_id)
            .await
            .map_err(|error| database(&error))?
        else {
            return Ok(false);
        };
        let my_follows: BTreeSet<i64> = graph.follows.iter().copied().collect();

        let neighbours = similarity::co_followers(&self.pool, user_id, NEIGHBOUR_LIMIT)
            .await
            .map_err(|error| database(&error))?;

        let weights = SimilarityWeights::default();
        let mut scored: Vec<ScoredNeighbour> = Vec::with_capacity(neighbours.len());
        for neighbour in neighbours {
            let Some(other) = similarity::graph(&self.pool, neighbour)
                .await
                .map_err(|error| database(&error))?
            else {
                // A neighbour can disappear between the self-join and this read
                // (soft delete, account purge). Skipping is correct: it is no longer
                // a candidate for propagation.
                debug!(user = %neighbour, "skipping a neighbour with no account row");
                continue;
            };
            let breakdown = compare(&graph.signals, &other.signals, weights);
            let score = blend(&breakdown, weights);
            if score >= MIN_SIMILARITY {
                scored.push(ScoredNeighbour {
                    user_id: neighbour,
                    graph: other,
                    breakdown,
                    similarity: score,
                });
            }
        }

        // Highest similarity first; the uuid tie-break keeps the stored top set
        // deterministic when float scores are equal.
        scored.sort_by(|a, b| {
            b.similarity
                .total_cmp(&a.similarity)
                .then_with(|| a.user_id.cmp(&b.user_id))
        });
        scored.truncate(MAX_SIMILAR_USERS);

        let neighbour_rows = build_neighbour_rows(user_id, &scored);
        let stored_neighbours =
            similarity::replace_neighbours(&self.pool, user_id, &neighbour_rows)
                .await
                .map_err(|error| database(&error))?;

        let mut candidate_rows = build_candidate_rows(&scored, &my_follows);
        candidate_rows.sort_by(|a, b| {
            b.affinity
                .total_cmp(&a.affinity)
                .then_with(|| a.account_id.cmp(&b.account_id))
        });
        candidate_rows.truncate(CANDIDATE_LIMIT);
        let stored_candidates = affinity::replace_for_user(&self.pool, user_id, &candidate_rows)
            .await
            .map_err(|error| database(&error))?;

        if neighbour_rows.is_empty() {
            info!(
                user = %user_id,
                "no overlapping users; the similarity and affinity snapshots are empty"
            );
        } else {
            info!(
                user = %user_id,
                neighbours = stored_neighbours,
                candidates = stored_candidates,
                "similarity pass finished"
            );
        }
        Ok(true)
    }
}

/// Build the `user_similarity` rows from the selected neighbours.
///
/// The database stores `real` (single precision) while the core math is `f64`;
/// narrowing here is deliberate and lossless enough for a ranking signal.
#[allow(clippy::cast_possible_truncation)]
fn build_neighbour_rows(user_id: Uuid, scored: &[ScoredNeighbour]) -> Vec<NeighbourRow> {
    scored
        .iter()
        .map(|neighbour| NeighbourRow {
            user_id,
            similar_user_id: neighbour.user_id,
            similarity: neighbour.similarity as f32,
            co_follow: neighbour.breakdown.co_follow as f32,
            co_fav: neighbour.breakdown.co_fav as f32,
            co_reply: neighbour.breakdown.co_reply as f32,
            reciprocity: neighbour.breakdown.reciprocity as f32,
        })
        .collect()
}

/// Propagate the neighbours' follows into candidate scores.
///
/// `score[c] += similarity(u, v) · follow_weight(c)` and `support[c] += 1` for every
/// stored neighbour `v` that follows `c`, excluding accounts the caller already
/// follows. Sorting and truncation happen in the caller so the ranking rule lives
/// in one place.
#[allow(clippy::cast_possible_truncation)]
fn build_candidate_rows(
    scored: &[ScoredNeighbour],
    my_follows: &BTreeSet<i64>,
) -> Vec<AffinityRow> {
    let mut totals: BTreeMap<i64, (f64, i16)> = BTreeMap::new();
    for neighbour in scored {
        for (account_id, follow_weight) in &neighbour.graph.follow_weights {
            if my_follows.contains(account_id) {
                continue;
            }
            let entry = totals.entry(*account_id).or_insert((0.0, 0));
            entry.0 += neighbour.similarity * follow_weight;
            entry.1 = entry.1.saturating_add(1);
        }
    }

    totals
        .into_iter()
        .map(|(account_id, (affinity, support))| AffinityRow {
            account_id,
            affinity: affinity as f32,
            support,
        })
        .collect()
}

impl Handler for RecomputeSimilarity {
    fn kind(&self) -> &'static str {
        "recompute_similarity"
    }

    fn run<'a>(
        &'a self,
        job: &'a Job,
    ) -> Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>> {
        Box::pin(async move {
            if let Some(subject) = job.subject_id.as_deref() {
                let user_id = subject.parse::<Uuid>().map_err(|error| {
                    JobError::permanent(format!("subject_id `{subject}` is not a uuid: {error}"))
                })?;
                if !self.recompute_one(user_id).await? {
                    return Err(JobError::permanent(format!(
                        "user {user_id} has no account row to build a graph from"
                    )));
                }
            } else {
                let users = similarity::recently_changed_users(&self.pool, MAINTENANCE_LIMIT)
                    .await
                    .map_err(|error| database(&error))?;
                info!(users = users.len(), "similarity maintenance pass started");
                for user_id in users {
                    if !self.recompute_one(user_id).await? {
                        debug!(user = %user_id, "skipping a user with no account row");
                    }
                }
            }
            Ok(())
        })
    }
}

/// Maps a database failure onto a retry: every write here is a replacement, so a
/// second attempt produces the same rows.
fn database(error: &sqlx::Error) -> JobError {
    JobError::retry(Duration::from_secs(30), format!("database: {error}"))
}
