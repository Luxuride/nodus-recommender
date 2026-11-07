//! The candidate model, and the baseline it has to beat.
//!
//! This is deliberately a second implementation of the production path: it runs over
//! an in-memory graph instead of SQL, because the question here is "would the ranking
//! have found what the user did next", and a query that only reads the materialised
//! tables would answer "what did the ranking already decide". Keeping the arithmetic
//! shared with `nodus-core` is what makes the comparison meaningful — same weights,
//! same damping, same similarity blend, different data.

use std::collections::{HashMap, HashSet};

use nodus_core::damping::damp_popularity;
use nodus_core::idf;
use nodus_core::similarity::{
    self, MAX_SIMILAR_USERS, MIN_SIMILARITY, SimilarityWeights, UserSignals,
};
use nodus_core::weighted_set::WeightedSet;

use super::split::Graph;

/// One user's signal set, built once and reused for every comparison.
#[derive(Debug)]
pub struct Prepared {
    signals: HashMap<i64, UserSignals>,
    weights: SimilarityWeights,
}

impl Prepared {
    /// Builds signal sets for every account that has training follows.
    #[must_use]
    pub fn new(graph: &Graph) -> Self {
        let mut signals = HashMap::new();
        for (account, followees) in &graph.training {
            let follows = followees
                .iter()
                .fold(WeightedSet::new(), |mut set, followee| {
                    set.insert(
                        account_key(*followee),
                        idf::follow_weight(graph.followers_of(*followee)),
                    );
                    set
                });
            signals.insert(
                *account,
                UserSignals {
                    follows,
                    ..UserSignals::default()
                },
            );
        }
        Self {
            signals,
            weights: SimilarityWeights::default(),
        }
    }

    /// The ranked candidate list for one account, best first.
    #[must_use]
    pub fn rank(&self, graph: &Graph, account: i64, k: usize) -> Vec<i64> {
        let already = graph
            .training_follows(account)
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let Some(own) = self.signals.get(&account) else {
            return Vec::new();
        };

        let neighbours = self.neighbours(account, own);
        if neighbours.is_empty() {
            return Vec::new();
        }

        let mut affinity: HashMap<i64, f64> = HashMap::new();
        for (similar, similarity) in neighbours {
            for followee in graph.training_follows(similar) {
                if *followee == account || already.contains(followee) {
                    continue;
                }
                *affinity.entry(*followee).or_default() +=
                    similarity * idf::follow_weight(graph.followers_of(*followee));
            }
        }

        let mut ranked: Vec<(i64, f64)> = affinity
            .into_iter()
            .map(|(candidate, score)| {
                (
                    candidate,
                    damp_popularity(score, graph.followers_of(candidate)),
                )
            })
            .collect();
        // Ties broken by account id so two runs print the same list.
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.into_iter().take(k).map(|(id, _)| id).collect()
    }

    /// The most similar users, strongest first, pruned at [`MIN_SIMILARITY`].
    fn neighbours(&self, account: i64, own: &UserSignals) -> Vec<(i64, f64)> {
        let mut scored: Vec<(i64, f64)> = self
            .signals
            .iter()
            .filter(|(other, _)| **other != account)
            .map(|(account, signals)| {
                let breakdown = similarity::compare(own, signals, self.weights);
                (*account, similarity::blend(&breakdown, self.weights))
            })
            .filter(|(_, similarity)| *similarity >= MIN_SIMILARITY)
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(MAX_SIMILAR_USERS);
        scored
    }
}

/// The popularity baseline: the accounts with the most followers that the user does
/// not already follow.
///
/// This is the honest straw man, not a weak one. On a follower graph it is what the
/// instance's own "discover" page does, and beating it is the only evidence that the
/// taste hop contributes anything.
#[must_use]
pub fn popularity(graph: &Graph, account: i64, k: usize) -> Vec<i64> {
    let already = graph
        .training_follows(account)
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let mut ranked: Vec<(i64, i64)> = graph
        .followers
        .iter()
        .filter(|(candidate, _)| **candidate != account && !already.contains(candidate))
        .map(|(candidate, followers)| (*candidate, *followers))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.into_iter().take(k).map(|(id, _)| id).collect()
}

/// Stable 64-bit key for a global account id.
///
/// `WeightedSet` is keyed by `u64` so it can be shared with status ids and other
/// opaque identities; account ids are small positive integers, so the cast is exact.
fn account_key(account: i64) -> u64 {
    u64::try_from(account).unwrap_or(0)
}
