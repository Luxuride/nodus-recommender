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

/// Lower bound on the uptake ratio in [`popularity_lift`].
///
/// A quarter is deliberately gentler than damping's celebrity compression: the
/// ratio is measured against our own users' behaviour, not against the global
/// follower count, so a large deviation already means the account is genuinely
/// mis-targeted rather than merely famous. The floor keeps the multiplier
/// bounded so one noisy bucket cannot erase a candidate.
pub const LIFT_FLOOR: f64 = 0.25;

/// Upper bound on the uptake ratio in [`popularity_lift`].
///
/// Four times the expected follow rate is generous, but a niche account with real
/// uptake is exactly the case the follower-count damping punishes, and the lift
/// exists to let it through. The ceiling stops a single lucky conversion in a
/// tiny bucket from dominating the list.
pub const LIFT_CEIL: f64 = 4.0;

/// Compress `score` by the candidate's follower count.
///
/// `score / (1 + ln(1 + n))^AFFINITY_DAMPING`. With zero followers the base is
/// exactly `1.0` and `1.0^0.5 == 1.0`, so the score is returned unchanged.
#[must_use]
pub fn damp_popularity(score: f64, followers_count: u32) -> f64 {
    let base = 1.0 + f64::from(followers_count).ln_1p();
    score / base.powf(AFFINITY_DAMPING)
}

/// Normalise affinity by how often accounts *like this* are followed by our users.
///
/// `affinity * (follow_rate / expected_rate)`, with the ratio clamped to
/// `LIFT_FLOOR..=LIFT_CEIL`. `follow_rate` is the observed follow-through for the
/// candidate's bucket (say, its follower-count decile) and `expected_rate` is what
/// that bucket would be expected to yield from exposure alone; the caller measures
/// both on our own interaction events. A ratio of `1.0` leaves affinity untouched.
///
/// # Difference from [`damp_popularity`]
///
/// Damping and lift answer different halves of the same question. Damping divides
/// by the global follower count, so it depresses *whatever everyone follows*: it is
/// cheap and needs no data, which is why it stays as a pre-filter on the graph
/// score, but it cannot tell a celebrity nobody in our sample converts on from a
/// niche account our users actually follow. Lift divides by what our own users
/// actually do, so a niche account with genuine uptake is no longer punished for
/// its follower count, and a hub that merely gets exposed is no longer rewarded for
/// it. Use damping when there is no interaction history, lift when there is.
///
/// Returns `affinity` unchanged when `expected_rate` is not strictly positive (no
/// usable baseline), and when any input is non-finite, so a missing measurement
/// degrades to the unadjusted score instead of poisoning it.
#[must_use]
pub fn popularity_lift(affinity: f64, follow_rate: f64, expected_rate: f64) -> f64 {
    if !affinity.is_finite()
        || !follow_rate.is_finite()
        || !expected_rate.is_finite()
        || expected_rate <= 0.0
    {
        return affinity;
    }
    let ratio = (follow_rate / expected_rate).clamp(LIFT_FLOOR, LIFT_CEIL);
    affinity * ratio
}

#[cfg(test)]
mod tests {
    use super::{AFFINITY_DAMPING, LIFT_CEIL, LIFT_FLOOR, damp_popularity, popularity_lift};

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

    #[test]
    fn lift_scales_affinity_by_observed_uptake() {
        assert!(approx(popularity_lift(0.5, 0.02, 0.01), 1.0));
        assert!(approx(popularity_lift(0.5, 0.005, 0.01), 0.25));
        assert!(approx(popularity_lift(0.5, 0.01, 0.01), 0.5));
    }

    #[test]
    fn lift_is_clamped_to_its_documented_bounds() {
        assert!(approx(popularity_lift(1.0, 1.0, 0.000_001), LIFT_CEIL));
        assert!(approx(popularity_lift(1.0, 0.0, 1.0), LIFT_FLOOR));
        assert!(approx(popularity_lift(1.0, 0.04, 0.01), LIFT_CEIL));
    }

    #[test]
    fn lift_leaves_affinity_alone_without_a_usable_baseline() {
        assert!(approx(popularity_lift(0.7, 0.5, 0.0), 0.7));
        assert!(approx(popularity_lift(0.7, 0.5, -1.0), 0.7));
        assert!(approx(popularity_lift(0.7, f64::NAN, 0.5), 0.7));
        assert!(approx(popularity_lift(0.7, 0.5, f64::INFINITY), 0.7));
        assert!(approx(popularity_lift(0.7, f64::NEG_INFINITY, 0.5), 0.7));
    }
}
