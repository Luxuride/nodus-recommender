//! Rejections as evidence: a stored strength that decays, instead of a verdict.

use super::dwell::shrink;
use crate::labels::EventKind;

/// Impressions of one account after which its penalty counts at half strength.
pub const PENALTY_CONFIDENCE_PRIOR: f64 = 3.0;

/// Half-life of a `dismiss` penalty, in days.
pub const DISMISS_HALF_LIFE_DAYS: f64 = 7.0;

/// Half-life of a `not_interested` penalty, in days.
pub const NOT_INTERESTED_HALF_LIFE_DAYS: f64 = 30.0;

/// The magnitude a fresh `not_interested` carries, in label units.
///
/// The normaliser between stored penalties and the ranker's `0..=1` input: a
/// penalty is divided by this so that `1.0` means "a fresh explicit rejection".
pub const PENALTY_NORMALISER: f64 = 5.0;

/// How strong a rejection is before decay and shrinkage.
///
/// [`PENALTY_NORMALISER`] for `not_interested`, and 60% of it for `dismiss`,
/// because a dismiss is a nudge ("show me less") while not-interested is an
/// instruction ("stop"). Positive events answer `0.0`: evidence for an account must
/// never subtract from its score.
#[must_use]
pub const fn base_penalty(kind: EventKind) -> f64 {
    match kind {
        EventKind::Dismiss => 3.0,
        EventKind::NotInterested => PENALTY_NORMALISER,
        EventKind::Impression
        | EventKind::Click
        | EventKind::Follow
        | EventKind::Boost
        | EventKind::Reply => 0.0,
    }
}

/// A stored penalty decayed to its value `age_days` after it was recorded.
///
/// `base · 0.5^(age / half_life)`. Decay happens at read time, so nothing has to
/// run a job to expire a rejection and the stored row never has to be rewritten.
#[must_use]
pub fn decayed_penalty(base: f64, age_days: f64, half_life_days: f64) -> f64 {
    if base <= 0.0 || half_life_days <= 0.0 {
        return 0.0;
    }
    let age = age_days.max(0.0);
    let decay = 0.5_f64.powf(age / half_life_days);
    (base * decay).clamp(0.0, base)
}

/// A stored penalty as the ranker wants it: decayed, shrunk, and normalised.
///
/// `0.0` means "no penalty left", `1.0` means "a fully observed explicit
/// rejection". Both decays (time) and shrinkage (thin evidence) apply, because one
/// impression must not be able to bury an account on its own.
#[must_use]
pub fn normalized_penalty(base: f64, age_days: f64, half_life_days: f64, observations: u32) -> f64 {
    let decayed = decayed_penalty(base, age_days, half_life_days);
    let shrunk = shrink(decayed, observations, PENALTY_CONFIDENCE_PRIOR);
    (shrunk / PENALTY_NORMALISER).clamp(0.0, 1.0)
}

/// Half-life of the penalty an event records, or `None` for positive events.
///
/// Only the two rejections decay; the positive labels are training targets and are
/// aged by the taste half-life instead, which is applied to the graph, not here.
#[must_use]
pub const fn penalty_half_life(kind: EventKind) -> Option<f64> {
    match kind {
        EventKind::Dismiss => Some(DISMISS_HALF_LIFE_DAYS),
        EventKind::NotInterested => Some(NOT_INTERESTED_HALF_LIFE_DAYS),
        EventKind::Impression
        | EventKind::Click
        | EventKind::Follow
        | EventKind::Boost
        | EventKind::Reply => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DISMISS_HALF_LIFE_DAYS, NOT_INTERESTED_HALF_LIFE_DAYS, PENALTY_CONFIDENCE_PRIOR,
        PENALTY_NORMALISER, base_penalty, decayed_penalty, normalized_penalty, penalty_half_life,
        shrink,
    };
    use crate::labels::EventKind;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn a_rejection_starts_at_its_stored_strength() {
        // Nothing positive is punitive, however strong the positive signal.
        assert!(approx(base_penalty(EventKind::Follow), 0.0));
        assert!(approx(base_penalty(EventKind::Impression), 0.0));
        // 3 / 5 of the unit for a dismiss: a nudge, not an instruction.
        assert!(approx(
            base_penalty(EventKind::Dismiss),
            0.6 * PENALTY_NORMALISER
        ));
        assert!(approx(
            base_penalty(EventKind::NotInterested),
            PENALTY_NORMALISER
        ));
    }

    #[test]
    fn one_impression_shrinks_a_rejection_to_a_quarter() {
        // 5 · 1/(1+3) / 5. Shrinkage is why the penalty is a fraction of the unit
        // even for a rejection given today (D27).
        let rejected = normalized_penalty(base_penalty(EventKind::NotInterested), 0.0, 30.0, 1);
        assert!(approx(rejected, 1.0 / (1.0 + 3.0)));
        let dismissed = normalized_penalty(base_penalty(EventKind::Dismiss), 0.0, 30.0, 1);
        assert!(approx(dismissed, 0.15));
    }

    #[test]
    fn a_rejection_approaches_the_unit_with_evidence() {
        // 5 · 25/28 / 5: shrinkage only approaches 1.0, it never reaches it.
        let observed = normalized_penalty(base_penalty(EventKind::NotInterested), 0.0, 30.0, 25);
        assert!(approx(observed, 25.0 / 28.0));
        assert!(observed < 1.0);
        assert!(observed > 0.89);
    }

    #[test]
    fn a_penalty_halves_once_per_half_life() {
        let base = 5.0;
        let half = decayed_penalty(
            base,
            NOT_INTERESTED_HALF_LIFE_DAYS,
            NOT_INTERESTED_HALF_LIFE_DAYS,
        );
        assert!(approx(half, base / 2.0));
        let quarter = decayed_penalty(
            base,
            2.0 * NOT_INTERESTED_HALF_LIFE_DAYS,
            NOT_INTERESTED_HALF_LIFE_DAYS,
        );
        assert!(approx(quarter, base / 4.0));
        // Negative age (a clock that went backwards) must not amplify a penalty.
        assert!(approx(decayed_penalty(base, -10.0, 7.0), base));
        assert!(approx(decayed_penalty(base, 0.0, 7.0), base));
    }

    #[test]
    fn a_dismiss_decays_faster_than_a_rejection() {
        assert!(approx(
            penalty_half_life(EventKind::Dismiss).expect("dismiss has a half-life"),
            DISMISS_HALF_LIFE_DAYS
        ));
        assert!(
            penalty_half_life(EventKind::NotInterested).expect("not interested decays")
                > penalty_half_life(EventKind::Dismiss).expect("dismiss decays")
        );
        assert!(penalty_half_life(EventKind::Follow).is_none());
        assert!(penalty_half_life(EventKind::Impression).is_none());
    }

    #[test]
    fn a_penalty_never_grows_and_never_turns_positive() {
        for age in [0.0, 1.0, 30.0, 3_650.0] {
            let value = decayed_penalty(5.0, age, 7.0);
            assert!(value >= 0.0);
            assert!(value <= 5.0);
        }
        assert!(approx(decayed_penalty(0.0, 1.0, 7.0), 0.0));
        assert!(approx(decayed_penalty(-3.0, 1.0, 7.0), 0.0));
        assert!(approx(decayed_penalty(5.0, 1.0, 0.0), 0.0));
    }

    #[test]
    fn a_normalised_penalty_is_a_fraction_of_a_rejection() {
        let half_life = NOT_INTERESTED_HALF_LIFE_DAYS;
        // A fresh rejection backed by plenty of evidence is the unit, approached
        // from below: shrinkage never reaches 1.0.
        let full = normalized_penalty(5.0, 0.0, half_life, 100_000);
        assert!(
            (0.99..=1.0).contains(&full),
            "a fresh rejection is the unit: {full}"
        );
        // Thin evidence keeps the same rejection quiet.
        let thin = normalized_penalty(5.0, 0.0, half_life, 1);
        assert!(
            thin < full / 3.0,
            "one impression must not bury an account: {thin}"
        );
        // Nothing recorded is nothing punitive.
        let none = normalized_penalty(0.0, 0.0, half_life, 50);
        assert!(approx(none, 0.0));
        // And time alone can take it below any threshold worth acting on.
        let aged = normalized_penalty(5.0, 10.0 * half_life, half_life, 100_000);
        assert!(aged < 0.01, "ten half-lives is a thousandth: {aged}");
    }

    #[test]
    fn one_impression_barely_penalises() {
        let raw = decayed_penalty(5.0, 0.0, NOT_INTERESTED_HALF_LIFE_DAYS);
        let observed = shrink(raw, 1, PENALTY_CONFIDENCE_PRIOR);
        assert!(
            observed < raw / 3.0,
            "one impression must not decide: {observed}"
        );
        assert!(shrink(raw, 50, PENALTY_CONFIDENCE_PRIOR) > raw * 0.9);
    }
}
