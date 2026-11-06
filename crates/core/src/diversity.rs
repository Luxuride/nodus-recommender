//! Diversity re-ranking: maximal marginal relevance plus a per-instance cap.
//!
//! `docs/06-ranking.md` §Diversity applies two constraints in order: at most
//! `MAX_PER_INSTANCE` accounts from one instance in any `INSTANCE_WINDOW`
//! consecutive items, then an MMR pass with `MMR_LAMBDA` that pushes down
//! candidates duplicating the taste of an already-selected account.

/// Relevance-versus-redundancy trade-off in [`mmr_select`].
///
/// `mmr = λ·score − (1−λ)·max_similarity`; the default `0.7` is
/// `docs/06-ranking.md` §Diversity.
pub const MMR_LAMBDA: f64 = 0.7;

/// Maximum number of items from one instance inside `INSTANCE_WINDOW` picks.
pub const MAX_PER_INSTANCE: usize = 2;

/// How many consecutive output picks the per-instance cap looks back over.
pub const INSTANCE_WINDOW: usize = 10;

/// Share of feed slots reserved for exploration (ranks 51–200 rather than the top).
///
/// `docs/06-ranking.md` §Exploration: without it the ranker can only confirm what
/// it already believes.
pub const EXPLORE_RATE: f64 = 0.05;

/// One re-rankable candidate.
///
/// The caller hashes `instance_host` to `u64` (the math layer never sees host
/// names) and supplies an embedding for redundancy; the embedding may be empty,
/// in which case every pair counts as dissimilar.
#[derive(Debug, Clone, PartialEq)]
pub struct Diversifiable {
    /// Stable identifier of the candidate.
    pub id: u64,
    /// Ranking score from [`crate::ranking`]; higher is better.
    pub score: f64,
    /// Instance the candidate belongs to, hashed to `u64` by the caller.
    pub instance_host: u64,
    /// Embedding used for redundancy; may be empty when none is available.
    pub embedding: Vec<f64>,
}

/// Choose at most `limit` indices from `items`, in output order.
///
/// Greedy MMR: each step picks the item maximizing
/// `lambda·score − (1−lambda)·max_similarity_to_already_chosen`, where similarity
/// is cosine over the embeddings and `0.0` when either embedding is empty. Items
/// whose instance already has `max_per_instance` picks among the last
/// `instance_window` picks are skipped.
///
/// An `instance_window` of `0` disables the cap; a `max_per_instance` of `0`
/// suppresses every candidate, so callers should pass at least `1`.
#[must_use]
pub fn mmr_select(
    items: &[Diversifiable],
    lambda: f64,
    max_per_instance: usize,
    instance_window: usize,
    limit: usize,
) -> Vec<usize> {
    let cap = limit.min(items.len());
    let diversity_weight = 1.0 - lambda;
    let mut selected: Vec<usize> = Vec::with_capacity(cap);
    let mut remaining: Vec<usize> = (0..items.len()).collect();

    while selected.len() < cap {
        let mut best: Option<(usize, f64)> = None;
        for (position, &index) in remaining.iter().enumerate() {
            let instance = items[index].instance_host;
            if instance_picks(items, &selected, instance, instance_window) >= max_per_instance {
                continue;
            }
            let redundancy = max_similarity(items, index, &selected);
            let value = lambda * items[index].score - diversity_weight * redundancy;
            let improves = best.is_none_or(|(_, best_value)| value > best_value);
            if improves {
                best = Some((position, value));
            }
        }

        match best {
            Some((position, _)) => {
                selected.push(remaining.remove(position));
            }
            None => break,
        }
    }

    selected
}

/// How many of the last `window` picks belong to `instance`.
fn instance_picks(
    items: &[Diversifiable],
    selected: &[usize],
    instance: u64,
    window: usize,
) -> usize {
    let start = selected.len().saturating_sub(window);
    selected[start..]
        .iter()
        .filter(|&&picked| items[picked].instance_host == instance)
        .count()
}

/// Highest cosine similarity between `candidate` and any already-selected item.
fn max_similarity(items: &[Diversifiable], candidate: usize, selected: &[usize]) -> f64 {
    let mut best = 0.0;
    for &picked in selected {
        let similarity = embedding_cosine(&items[candidate].embedding, &items[picked].embedding);
        if similarity > best {
            best = similarity;
        }
    }
    best
}

/// Cosine similarity between two embeddings, `0.0` if either is empty or mismatched.
fn embedding_cosine(a: &[f64], b: &[f64]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }

    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;
    for (&x, &y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    if norm_a <= 0.0 || norm_b <= 0.0 {
        return 0.0;
    }

    (dot / (norm_a.sqrt() * norm_b.sqrt())).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::{Diversifiable, INSTANCE_WINDOW, MAX_PER_INSTANCE, MMR_LAMBDA, mmr_select};

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn item(id: u64, score: f64, instance: u64) -> Diversifiable {
        Diversifiable {
            id,
            score,
            instance_host: instance,
            embedding: Vec::new(),
        }
    }

    #[test]
    fn respects_the_instance_cap_inside_the_window() {
        let items: Vec<Diversifiable> = (0..5u32)
            .map(|i| item(u64::from(i), 1.0 - 0.1 * f64::from(i), 7))
            .collect();
        let picked = mmr_select(&items, MMR_LAMBDA, 2, INSTANCE_WINDOW, 5);
        assert_eq!(picked.len(), 2);
        assert!(approx(items[picked[0]].score, 1.0));
    }

    #[test]
    fn returns_at_most_limit_indices() {
        let items: Vec<Diversifiable> = (0..10u64).map(|i| item(i, 1.0, i)).collect();
        let picked = mmr_select(&items, MMR_LAMBDA, MAX_PER_INSTANCE, INSTANCE_WINDOW, 3);
        assert_eq!(picked.len(), 3);
    }

    #[test]
    fn limit_larger_than_input_returns_everything() {
        let items: Vec<Diversifiable> = (0..4u64).map(|i| item(i, 1.0, i)).collect();
        let picked = mmr_select(&items, MMR_LAMBDA, MAX_PER_INSTANCE, INSTANCE_WINDOW, 100);
        assert_eq!(picked, vec![0, 1, 2, 3]);
    }

    #[test]
    fn orders_by_score_when_lambda_is_one() {
        let items = vec![item(0, 0.1, 1), item(1, 0.9, 2), item(2, 0.5, 3)];
        let picked = mmr_select(&items, 1.0, MAX_PER_INSTANCE, INSTANCE_WINDOW, 3);
        assert_eq!(picked, vec![1, 2, 0]);
    }

    #[test]
    fn sliding_window_lets_an_instance_reappear() {
        let items = vec![item(0, 0.9, 1), item(1, 0.8, 1), item(2, 0.7, 2)];

        let wide = mmr_select(&items, 1.0, 1, 10, 3);
        assert_eq!(wide, vec![0, 2]);

        let narrow = mmr_select(&items, 1.0, 1, 1, 3);
        assert_eq!(narrow, vec![0, 2, 1]);
    }

    #[test]
    fn redundant_embeddings_are_pushed_down() {
        let items = vec![
            Diversifiable {
                id: 0,
                score: 0.9,
                instance_host: 1,
                embedding: vec![1.0, 0.0],
            },
            Diversifiable {
                id: 1,
                score: 0.8,
                instance_host: 2,
                embedding: vec![1.0, 0.0],
            },
            Diversifiable {
                id: 2,
                score: 0.5,
                instance_host: 3,
                embedding: vec![0.0, 1.0],
            },
        ];
        let picked = mmr_select(&items, 0.5, MAX_PER_INSTANCE, INSTANCE_WINDOW, 3);
        assert_eq!(picked, vec![0, 2, 1]);
    }

    #[test]
    fn empty_embeddings_never_penalize() {
        let items = vec![item(0, 0.9, 1), item(1, 0.8, 2), item(2, 0.7, 3)];
        let picked = mmr_select(&items, MMR_LAMBDA, MAX_PER_INSTANCE, INSTANCE_WINDOW, 3);
        assert_eq!(picked, vec![0, 1, 2]);
    }
}
