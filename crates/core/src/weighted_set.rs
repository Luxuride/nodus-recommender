//! IDF-weighted id sets and their cosine overlap.
//!
//! A [`WeightedSet`] is the in-memory shape of one signal for one user: the ids
//! they follow, favourited or replied to, each with the inverse-popularity weight
//! from [`crate::idf`]. Similarity in `docs/05-similarity.md` §User–user
//! similarity is cosine similarity over these sets.

use std::collections::BTreeMap;

/// A map from id to weight that keeps the maximum weight per id.
///
/// Backed by a `BTreeMap` so iteration order — and therefore every cosine sum —
/// is deterministic. The inner map is private: build one with [`WeightedSet::new`]
/// and [`WeightedSet::insert`], which is what the sync layer does.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WeightedSet(BTreeMap<u64, f64>);

impl WeightedSet {
    /// Create an empty set.
    #[must_use]
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Insert `weight` for `id`, keeping the larger weight if `id` is present.
    ///
    /// Duplicate ids do occur in imports (two sync pages covering the same
    /// follow); taking the max means a re-import can never weaken a signal.
    pub fn insert(&mut self, id: u64, weight: f64) {
        self.0
            .entry(id)
            .and_modify(|existing| *existing = existing.max(weight))
            .or_insert(weight);
    }

    /// Number of distinct ids in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set holds no ids.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Cosine similarity over the weighted ids.
    ///
    /// `dot / (sqrt(norm_a) * sqrt(norm_b))`. Returns `0.0` when either side is
    /// empty or a norm is zero, and clamps into `0.0..=1.0` so float noise cannot
    /// produce `1.0000000000000002`.
    #[must_use]
    pub fn cosine(&self, other: &Self) -> f64 {
        if self.0.is_empty() || other.0.is_empty() {
            return 0.0;
        }

        let mut dot = 0.0;
        let mut norm_self = 0.0;
        for (&id, &weight) in &self.0 {
            norm_self += weight * weight;
            if let Some(&other_weight) = other.0.get(&id) {
                dot += weight * other_weight;
            }
        }

        let mut norm_other = 0.0;
        for &weight in other.0.values() {
            norm_other += weight * weight;
        }

        if norm_self <= 0.0 || norm_other <= 0.0 {
            return 0.0;
        }

        (dot / (norm_self.sqrt() * norm_other.sqrt())).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::WeightedSet;

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

    #[test]
    fn identical_sets_have_cosine_one() {
        let a = set(&[(1, 0.5), (2, 0.25), (3, 1.0)]);
        let b = set(&[(1, 0.5), (2, 0.25), (3, 1.0)]);
        assert!(approx(a.cosine(&b), 1.0));
    }

    #[test]
    fn disjoint_sets_have_cosine_zero() {
        let a = set(&[(1, 0.5), (2, 0.25)]);
        let b = set(&[(3, 0.5), (4, 0.25)]);
        assert!(approx(a.cosine(&b), 0.0));
    }

    #[test]
    fn empty_sets_have_cosine_zero() {
        let empty = WeightedSet::new();
        let full = set(&[(1, 0.5)]);
        assert!(approx(empty.cosine(&full), 0.0));
        assert!(approx(full.cosine(&empty), 0.0));
        assert!(approx(empty.cosine(&empty), 0.0));
    }

    #[test]
    fn cosine_is_clamped_into_unit_range() {
        let a = set(&[(1, 0.1), (2, 0.2), (3, 0.3), (4, 0.7), (5, 0.9)]);
        let b = set(&[(1, 0.1), (2, 0.2), (3, 0.3), (4, 0.7), (5, 0.9)]);
        let cosine = a.cosine(&b);
        assert!(cosine <= 1.0);
        assert!(cosine >= 0.0);
        assert!(approx(cosine, 1.0));
    }

    #[test]
    fn repeated_insert_keeps_the_maximum_weight() {
        let mut a = WeightedSet::new();
        a.insert(1, 0.5);
        a.insert(1, 0.2);
        assert_eq!(a.len(), 1);
        assert!(approx(a.cosine(&set(&[(1, 0.5)])), 1.0));
    }
}
