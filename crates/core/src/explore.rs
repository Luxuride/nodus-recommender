//! Explore slots: the small, deliberate part of a feed nobody ranked highly.
//!
//! A feed built only from the top of the ranking is a closed loop. Accounts that
//! were rejected once and are decaying never come back, because nothing can produce
//! positive evidence about an account nobody is shown; new accounts never leave the
//! tail, because they have no signals to rank with. Exploration is the mechanism that
//! breaks both loops, and it is a *required* part of the design rather than a tuning
//! knob — see decision D26, which depends on this channel to make decay mean anything.
//!
//! Two rules keep it honest:
//!
//! * explore items come from ranks below the head, so they never displace the best
//!   answers (a slot taken at rank 3 is a slot taken from the user's strongest signal);
//! * the choice is deterministic in the feed's seed, so a scroll that re-reads page 1
//!   sees the same list, and two replicas serving the same snapshot agree.

/// Share of a page given to exploration, borrowed from [`crate::diversity::EXPLORE_RATE`]
/// so the two channels that widen a feed — diversity re-ranking and exploration — cannot
/// drift into disagreeing about how much of a page is spent on anything but the head.
pub use crate::diversity::EXPLORE_RATE;

/// First rank exploration may draw from. Below this, the ranking is trustworthy.
pub const EXPLORE_FIRST_RANK: usize = 50;

/// Last rank exploration may draw from. Past this the tail is not "unlucky", it is
/// bad, and showing it costs a user's trust to learn nothing.
pub const EXPLORE_LAST_RANK: usize = 200;

/// How many explore slots a page of `limit` items gets.
///
/// At least one for any page long enough to spare it, because a rate that rounds to
/// zero on every page never fires at all: five percent of 20 items is one item, but
/// five percent of a 12-item page is 0.6 and rounding down would silence the channel
/// for the whole session.
#[must_use]
pub fn slots(limit: usize) -> usize {
    if limit < 4 {
        // A three-item page has no spare slot, and taking one would be the whole page.
        return 0;
    }
    // `(limit + 10) / 20` is `round(limit · 0.05)` in integers: 20 → 1, 40 → 2,
    // 100 → 5. Doing it in `f64` would need three cast lints to say the same thing.
    ((limit + 10) / 20).max(1)
}

/// Choose `count` distinct indices from `0..pool`, deterministically for `seed`.
///
/// Partial Fisher–Yates over a virtual index list: the list is the identity
/// permutation, and a swap is recorded in a map instead of being applied to an array,
/// so the cost is `O(count)` rather than `O(pool)`. That matters because the pool is
/// hundreds of entries and this runs on every first page.
///
/// The stream is `SplitMix64`: uniform enough for a selection this small, deterministic
/// for a given seed, and free of a random-number dependency — which is not fussiness,
/// because this crate also compiles to wasm.
#[must_use]
pub fn choose(pool: usize, count: usize, seed: u64) -> Vec<usize> {
    use std::collections::BTreeMap;

    let count = count.min(pool);
    let mut state = seed;
    let mut swaps: BTreeMap<usize, usize> = BTreeMap::new();
    let mut chosen = Vec::with_capacity(count);

    for step in 0..count {
        let remaining = pool - step;
        // Modulo bias is `remaining / 2^64`; for a pool of a few hundred that is a
        // rounding error, not a bias.
        let offset = usize::try_from(next(&mut state) % remaining as u64).unwrap_or(0);
        let pick = step + offset;

        let at_step = swaps.get(&step).copied().unwrap_or(step);
        let at_pick = swaps.get(&pick).copied().unwrap_or(pick);
        swaps.insert(step, at_pick);
        swaps.insert(pick, at_step);
        chosen.push(at_pick);
    }

    chosen
}

/// `SplitMix64`. Small, deterministic and adequate for picking a handful of indices.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_page_gets_no_explore_slot() {
        assert_eq!(slots(3), 0);
        assert_eq!(slots(0), 0);
    }

    #[test]
    fn a_normal_page_gets_at_least_one() {
        assert_eq!(slots(4), 1);
        assert_eq!(slots(20), 1);
        assert_eq!(slots(40), 2);
        assert_eq!(slots(100), 5);
    }

    #[test]
    fn the_same_seed_gives_the_same_choice() {
        assert_eq!(choose(150, 3, 42), choose(150, 3, 42));
    }

    #[test]
    fn different_seeds_give_different_choices() {
        // Not a statistical claim, just that the seed is actually used.
        assert_ne!(choose(150, 3, 42), choose(150, 3, 43));
    }

    #[test]
    fn choices_are_distinct_and_in_range() {
        for seed in 0..25 {
            let picked = choose(150, 5, seed);
            assert_eq!(picked.len(), 5);
            let mut sorted = picked.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), 5, "duplicates in {picked:?}");
            assert!(picked.iter().all(|index| *index < 150));
        }
    }

    #[test]
    fn asking_for_more_than_the_pool_returns_the_pool() {
        let picked = choose(3, 10, 7);
        assert_eq!(picked.len(), 3);
        let mut sorted = picked;
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2]);
    }
}
