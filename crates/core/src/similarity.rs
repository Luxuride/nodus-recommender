//! User–user similarity and candidate-generation weights.
//!
//! Mirrors `docs/05-similarity.md`: each signal is a cosine over IDF-weighted id
//! sets, combined linearly into one similarity in `[0, 1]`, which then propagates
//! similar users' follows into candidates.

use crate::weighted_set::WeightedSet;

/// Relative weight of each similarity component.
///
/// The defaults are the initial values from `docs/05-similarity.md` §Signals and
/// sum to exactly `1.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimilarityWeights {
    /// Co-follow overlap (default `0.60`), the strongest cheap signal.
    pub co_follow: f64,
    /// Co-favourite overlap (default `0.25`): sharing a favourited post is rarer.
    pub co_fav: f64,
    /// Co-reply overlap (default `0.10`): arguing with the same people.
    pub co_reply: f64,
    /// Reciprocity component (default `0.05`): a weak tie-strength hint.
    pub reciprocity: f64,
}

impl Default for SimilarityWeights {
    fn default() -> Self {
        Self {
            co_follow: 0.60,
            co_fav: 0.25,
            co_reply: 0.10,
            reciprocity: 0.05,
        }
    }
}

/// Per-component similarity values, each in `[0, 1]`.
///
/// Kept separate from the blended score so the UI can explain which signal fired
/// ("you both follow @x") and so the offline evaluation can inspect components.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimilarityBreakdown {
    /// Cosine over the two users' follow sets.
    pub co_follow: f64,
    /// Cosine over the two users' favourited-status sets.
    pub co_fav: f64,
    /// Cosine over the two users' replied-to-account sets.
    pub co_reply: f64,
    /// Cosine over the followed-account sets, reused as the mutual-interest term.
    pub reciprocity: f64,
}

/// One user's signal sets, already weighted by [`crate::idf`].
///
/// `follows` holds followed account ids, `favourites` holds favourited status
/// ids, and `replies` holds the account ids the user replied to.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserSignals {
    /// Followed accounts, weighted by `follow_weight`.
    pub follows: WeightedSet,
    /// Favourited statuses, weighted by `status_weight`.
    pub favourites: WeightedSet,
    /// Accounts replied to, weighted by `target_weight`.
    pub replies: WeightedSet,
}

/// Compare two users signal by signal.
///
/// Each component is a cosine over the corresponding [`WeightedSet`].
/// `reciprocity` is the mutual-interest component. v1 has only undirected follow
/// sets here, so it reuses the `follows` sets: it reflects that both users follow
/// overlapping accounts, not that they follow each other.
///
/// The components are unweighted; the argument is accepted for signature symmetry
/// with [`blend`], which applies the weights.
#[must_use]
pub fn compare(
    a: &UserSignals,
    b: &UserSignals,
    _weights: SimilarityWeights,
) -> SimilarityBreakdown {
    let follow_overlap = a.follows.cosine(&b.follows);
    SimilarityBreakdown {
        co_follow: follow_overlap,
        co_fav: a.favourites.cosine(&b.favourites),
        co_reply: a.replies.cosine(&b.replies),
        reciprocity: follow_overlap,
    }
}

/// Blend a breakdown into a single similarity in `[0, 1]`.
///
/// `w.co_follow·co_follow + w.co_fav·co_fav + w.co_reply·co_reply +
/// w.reciprocity·reciprocity`.
#[must_use]
pub fn blend(breakdown: &SimilarityBreakdown, weights: SimilarityWeights) -> f64 {
    let mut total = weights.co_follow * breakdown.co_follow;
    total += weights.co_fav * breakdown.co_fav;
    total += weights.co_reply * breakdown.co_reply;
    total += weights.reciprocity * breakdown.reciprocity;
    total
}

/// Similar user pairs below this score are not stored.
///
/// See `docs/05-similarity.md` §User–user similarity.
pub const MIN_SIMILARITY: f64 = 0.05;

/// How many similar users are kept per user and used for propagation.
///
/// See `docs/05-similarity.md` §Candidate generation.
pub const MAX_SIMILAR_USERS: usize = 50;

/// How many candidates per user are persisted in `account_affinity`.
///
/// See `docs/05-similarity.md` §Output.
pub const CANDIDATE_LIMIT: usize = 500;

/// Weight of the two-hop graph source, `0.25 · w_follow(i) · w_follow(c)`.
///
/// See `docs/05-similarity.md` §Candidate generation.
pub const TWO_HOP_WEIGHT: f64 = 0.25;

/// Weight of the co-engagement source, `0.35 · Σ w_engager(a)`.
///
/// See `docs/05-similarity.md` §Candidate generation.
pub const CO_ENGAGEMENT_WEIGHT: f64 = 0.35;

#[cfg(test)]
mod tests {
    use super::{SimilarityWeights, UserSignals, blend, compare};
    use crate::weighted_set::WeightedSet;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn set(entries: &[(u64, f64)]) -> WeightedSet {
        let mut built = WeightedSet::new();
        for &(id, weight) in entries {
            built.insert(id, weight);
        }
        built
    }

    fn signals() -> UserSignals {
        UserSignals {
            follows: set(&[(1, 0.5), (2, 0.25), (3, 1.0)]),
            favourites: set(&[(10, 0.3), (11, 0.6)]),
            replies: set(&[(100, 0.7)]),
        }
    }

    #[test]
    fn identical_users_similarity_one() {
        let a = signals();
        let b = a.clone();
        let weights = SimilarityWeights::default();
        let breakdown = compare(&a, &b, weights);

        assert!(approx(breakdown.co_follow, 1.0));
        assert!(approx(breakdown.co_fav, 1.0));
        assert!(approx(breakdown.co_reply, 1.0));
        assert!(approx(breakdown.reciprocity, 1.0));
        assert!(approx(blend(&breakdown, weights), 1.0));
    }

    #[test]
    fn default_weights_sum_to_one() {
        let weights = SimilarityWeights::default();
        let sum = weights.co_follow + weights.co_fav + weights.co_reply + weights.reciprocity;
        assert!((sum - 1.0).abs() < 1e-12);
    }

    #[test]
    fn blended_weights_are_the_defaults() {
        let weights = SimilarityWeights::default();
        assert!(approx(weights.co_follow, 0.60));
        assert!(approx(weights.co_fav, 0.25));
        assert!(approx(weights.co_reply, 0.10));
        assert!(approx(weights.reciprocity, 0.05));
    }
}
