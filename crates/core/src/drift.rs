//! Taste drift and refresh stability for account ranking.
//!
//! Two forces pull in opposite directions that `docs/06-ranking.md` does not yet
//! model. A recommendation list has to follow a user's taste as it changes: a
//! follow from three years ago is weaker evidence about who the user is now than
//! one from last month. At the same time the list has to look steady between
//! refreshes, because a list that reshuffles for no visible reason reads as a
//! list that does not know what it is doing, even when every individual score
//! change is defensible.
//!
//! [`recency_weight`] and [`weighted_follow`] implement the first;
//! [`stability_bonus`] implements the second. Everything here is a pure function
//! of its arguments like the rest of the crate: the caller reads its own clock,
//! computes the ages, and passes them in.

/// Time constant of taste decay, in days: `exp(-age_days / 180)`.
///
/// Half a year is roughly the horizon over which a follow stops describing who a
/// user is now: taste in accounts moves on the scale of months, not days (that is
/// the freshness term) and not years (that is the whole graph). The name is a
/// time constant, not a half-life — the weight is `1/e ≈ 0.37` at 180 days and
/// `0.5` at `TASTE_TAU_DAYS · ln 2 ≈ 125` days. It is the "half-life-ish" knob
/// for how fast a follow fades, and it is deliberately much slower than
/// `STABILITY_TAU_DAYS`, because taste should outlive the exposure window.
pub const TASTE_TAU_DAYS: f64 = 180.0;

/// Time constant of the refresh-stability bonus, in days: `exp(-age_days / 7)`.
///
/// One week matches the fatigue window, so "shown recently" means the same
/// seven days the fatigue term already counts. Splitting the two windows would
/// let an account be stale enough to lose its stability bonus while still being
/// recent enough to accrue fatigue.
pub const STABILITY_TAU_DAYS: f64 = 7.0;

/// How much of a follow's evidence survives `age_days` of taste drift.
///
/// `exp(-age_days / TASTE_TAU_DAYS)`, clamped to `0.0..=1.0`. Negative ages mean
/// the caller's clock is ahead of the stored timestamp; a clock skew must not
/// boost a signal, so they return `1.0` rather than a value above one. A
/// non-finite age carries no usable information and returns `0.0`.
#[must_use]
pub fn recency_weight(age_days: f64) -> f64 {
    if !age_days.is_finite() {
        return 0.0;
    }
    if age_days < 0.0 {
        return 1.0;
    }
    (-age_days / TASTE_TAU_DAYS).exp().clamp(0.0, 1.0)
}

/// An IDF-weighted follow, discounted by how long ago it was created.
///
/// `idf_weight * recency_weight(first_seen_days_ago)`. A follow from three years
/// ago says less about who you are now than one from last month, and
/// `TASTE_TAU_DAYS` is the half-life-ish knob for how fast that fades. `idf_weight`
/// is normally [`crate::idf::follow_weight`], already scaled so that following a
/// ubiquitous account counts for less.
#[must_use]
pub fn weighted_follow(first_seen_days_ago: f64, idf_weight: f64) -> f64 {
    idf_weight * recency_weight(first_seen_days_ago)
}

/// What the list already knows about a candidate's exposure to this user.
///
/// `shown_age_days` is `None` when the candidate has never been shown; the bonus
/// is then exactly zero. `previous_rank` is carried for the caller (and for a
/// future tie-break) so a consumer can tell "new" from "moved" without a schema
/// change; [`stability_bonus`] itself does not read it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stability {
    /// Days since this candidate was last shown to the user, if ever.
    pub shown_age_days: Option<f64>,
    /// Rank the candidate held on the previous refresh, if it was ranked.
    pub previous_rank: Option<u32>,
}

/// Small bonus for a candidate the user was already shown recently.
///
/// `lambda * exp(-shown_age_days / STABILITY_TAU_DAYS)`, clamped to
/// `0.0..=lambda`. Churn reads as unreliability to a user even when every
/// individual score change is defensible: an account that was worth showing
/// yesterday and is still close in score is usually still worth showing, and
/// pinning it costs the list almost nothing because the bonus is bounded by
/// `lambda`. It never exceeds `lambda`, so a stable account cannot outrank one
/// with genuinely better evidence.
///
/// Returns `0.0` when the candidate was never shown, when the stored age is
/// non-finite, or when `lambda` is non-positive or non-finite (a disabled or
/// broken weight must not panic inside `f64::clamp`).
#[must_use]
pub fn stability_bonus(stability: &Stability, lambda: f64) -> f64 {
    let Some(shown_age_days) = stability.shown_age_days else {
        return 0.0;
    };
    if !shown_age_days.is_finite() || !lambda.is_finite() || lambda <= 0.0 {
        return 0.0;
    }
    // A negative age (clock skew) makes the decay exceed one; the clamp below
    // turns that into the full `lambda`, never more.
    (lambda * (-shown_age_days / STABILITY_TAU_DAYS).exp()).clamp(0.0, lambda)
}

#[cfg(test)]
mod tests {
    use super::{
        STABILITY_TAU_DAYS, Stability, TASTE_TAU_DAYS, recency_weight, stability_bonus,
        weighted_follow,
    };

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn recency_decays_monotonically_with_age() {
        let mut previous = recency_weight(0.0);
        assert!(approx(previous, 1.0));
        for age in 1..=360 {
            let current = recency_weight(f64::from(age));
            assert!(current < previous);
            previous = current;
        }
        assert!(approx(recency_weight(TASTE_TAU_DAYS), (-1.0_f64).exp()));
    }

    #[test]
    fn clock_skew_cannot_boost_a_signal() {
        assert!(approx(recency_weight(-1.0), 1.0));
        assert!(approx(recency_weight(-10_000.0), 1.0));
    }

    #[test]
    fn non_finite_ages_carry_no_weight() {
        assert!(approx(recency_weight(f64::NAN), 0.0));
        assert!(approx(recency_weight(f64::INFINITY), 0.0));
        assert!(approx(recency_weight(f64::NEG_INFINITY), 0.0));
    }

    #[test]
    fn weighted_follow_is_idf_times_recency() {
        let idf = 0.8;
        assert!(approx(weighted_follow(0.0, idf), idf));
        assert!(approx(
            weighted_follow(TASTE_TAU_DAYS, idf),
            idf * (-1.0_f64).exp()
        ));
        assert!(approx(weighted_follow(-30.0, idf), idf));
    }

    #[test]
    fn stability_bonus_is_bounded_by_lambda() {
        let lambda = 0.15;
        let shown = Stability {
            shown_age_days: Some(0.0),
            previous_rank: Some(1),
        };
        assert!(approx(stability_bonus(&shown, lambda), lambda));

        let a_week = Stability {
            shown_age_days: Some(STABILITY_TAU_DAYS),
            previous_rank: None,
        };
        assert!(approx(
            stability_bonus(&a_week, lambda),
            lambda * (-1.0_f64).exp()
        ));

        let ancient = Stability {
            shown_age_days: Some(10_000.0),
            previous_rank: None,
        };
        assert!((0.0..lambda).contains(&stability_bonus(&ancient, lambda)));

        let skewed = Stability {
            shown_age_days: Some(-5.0),
            previous_rank: None,
        };
        assert!(approx(stability_bonus(&skewed, lambda), lambda));
    }

    #[test]
    fn a_never_shown_account_gets_no_bonus() {
        let novel = Stability {
            shown_age_days: None,
            previous_rank: None,
        };
        assert!(approx(stability_bonus(&novel, 0.15), 0.0));
        assert!(approx(stability_bonus(&novel, 0.0), 0.0));
        assert!(approx(stability_bonus(&novel, f64::NAN), 0.0));
    }

    #[test]
    fn a_disabled_lambda_does_not_panic_or_pay_out() {
        let shown = Stability {
            shown_age_days: Some(0.0),
            previous_rank: None,
        };
        assert!(approx(stability_bonus(&shown, 0.0), 0.0));
        assert!(approx(stability_bonus(&shown, -1.0), 0.0));
        assert!(approx(stability_bonus(&shown, f64::INFINITY), 0.0));
        let broken_age = Stability {
            shown_age_days: Some(f64::NAN),
            previous_rank: None,
        };
        assert!(approx(stability_bonus(&broken_age, 0.15), 0.0));
    }
}
