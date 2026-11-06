//! Inverse-popularity weights for set-overlap signals.
//!
//! See `docs/05-similarity.md` §Signals. An account followed by a million people
//! says almost nothing about shared taste, so every overlap is weighted by
//! `1 / ln(e + count)`. The `e` offset keeps a zero count finite: with
//! `count = 0` the denominator is `ln(e) = 1`, so the weight is exactly `1.0`
//! instead of diverging as a bare `1 / ln(count)` would.

use std::f64::consts::E;

/// Weight of a followee, from its follower count.
///
/// `1 / ln(e + followers_count)`, the `w_follow` term of
/// `docs/05-similarity.md`. A zero follower count yields exactly `1.0`
/// (`ln(e) = 1`), so brand-new accounts still count, just at full weight.
#[must_use]
pub fn follow_weight(followers_count: u32) -> f64 {
    inverse_frequency(followers_count)
}

/// Weight of a favourited status, from its favourite count.
///
/// `1 / ln(e + favourites_count)`, the `w_status` term of
/// `docs/05-similarity.md`. A zero count yields exactly `1.0`.
#[must_use]
pub fn status_weight(favourites_count: u32) -> f64 {
    inverse_frequency(favourites_count)
}

/// Weight of an account that people reply to, from its distinct engager count.
///
/// `1 / ln(e + distinct_engagers)`, the `w_target` term of
/// `docs/05-similarity.md`. A zero count yields exactly `1.0`.
#[must_use]
pub fn target_weight(distinct_engagers: u32) -> f64 {
    inverse_frequency(distinct_engagers)
}

/// Shared shape of the three weights: `1 / ln(e + count)`.
///
/// `ln(e + 0) = 1` is what makes the zero-count case finite rather than a
/// division by zero.
fn inverse_frequency(count: u32) -> f64 {
    1.0 / (E + f64::from(count)).ln()
}

#[cfg(test)]
mod tests {
    use std::f64::consts::E;

    use super::{follow_weight, status_weight, target_weight};

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn zero_counts_weigh_exactly_one() {
        assert_eq!(follow_weight(0), 1.0);
        assert_eq!(status_weight(0), 1.0);
        assert_eq!(target_weight(0), 1.0);
    }

    #[test]
    fn weights_decrease_as_counts_grow() {
        let weights: [fn(u32) -> f64; 3] = [follow_weight, status_weight, target_weight];
        for weight in weights {
            assert!(weight(10) > weight(1_000));
            assert!(weight(1_000) > weight(1_000_000));
            assert!(weight(10) < 1.0);
            assert!(approx(weight(1), 1.0 / (E + 1.0).ln()));
        }
    }
}
