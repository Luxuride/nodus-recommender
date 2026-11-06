//! Popularity damping for candidate scores.
//!
//! `docs/05-similarity.md` §Popularity damping: a candidate that looks like a
//! celebrity has its score compressed, otherwise every feed is the same ten
//! famous accounts.

/// Exponent applied to the damping base, `(1 + ln(1 + n))^AFFINITY_DAMPING`.
///
/// The default `0.5` is a square root; `0.0` disables damping entirely, which is
/// useful for A/B testing rather than production.
pub const AFFINITY_DAMPING: f64 = 0.5;

/// Compress `score` by the candidate's follower count.
///
/// `score / (1 + ln(1 + n))^AFFINITY_DAMPING`. With zero followers the base is
/// exactly `1.0` and `1.0^0.5 == 1.0`, so the score is returned unchanged.
#[must_use]
pub fn damp_popularity(score: f64, followers_count: u32) -> f64 {
    let base = 1.0 + f64::from(followers_count).ln_1p();
    score / base.powf(AFFINITY_DAMPING)
}

#[cfg(test)]
mod tests {
    use super::{AFFINITY_DAMPING, damp_popularity};

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn zero_followers_leaves_the_score_unchanged() {
        assert_eq!(damp_popularity(0.42, 0), 0.42);
        assert_eq!(damp_popularity(-1.5, 0), -1.5);
    }

    #[test]
    fn more_followers_compress_the_score_more() {
        let small = damp_popularity(1.0, 100);
        let large = damp_popularity(1.0, 1_000_000);
        assert!(small < 1.0);
        assert!(large < small);
    }

    #[test]
    fn exponent_is_the_documented_square_root() {
        assert!(approx(AFFINITY_DAMPING, 0.5));
        let expected = 1.0 / (1.0 + 9.0_f64.ln_1p()).powf(0.5);
        assert!(approx(damp_popularity(1.0, 9), expected));
    }
}
