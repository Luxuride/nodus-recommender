//! How much weight one observation is allowed to carry.
//!
//! Three ideas live here, and they exist because a recommender is an easy machine
//! to make unstable:
//!
//! * **Bounded corrections.** A dwell time is evidence about attention, not a
//!   verdict. It adjusts a label by at most [`DWELL_MAX_CORRECTION`], so it can
//!   never turn a follow into a dismissal.
//! * **Decaying penalties.** A rejection is evidence too, and evidence about a
//!   person gets stale. Penalties halve on a documented schedule instead of
//!   excluding an account forever.
//! * **Shrinkage.** Every correction is scaled by [`confidence`], so a user with
//!   four impressions does not get a feed that reshuffles on each scroll. Thin
//!   evidence means a quiet feed, not a responsive one.
//!
//! None of these know about HTTP, the database or the wire format; they are pure
//! functions so the behaviour is testable without a running system.

use crate::labels::EventKind;

/// Weight of the dwell correction inside a label.
pub const DWELL_WEIGHT: f64 = 0.35;

/// Hard bound on a dwell correction.
///
/// Half of a click (`1.0`), so attention can never outvote an action.
pub const DWELL_MAX_CORRECTION: f64 = 0.5;

/// Longest dwell that is believed, in milliseconds.
///
/// A tab left open overnight reports hours of "attention"; capping at two minutes
/// keeps the signal, which saturates long before that.
pub const DWELL_CAP_MS: u32 = 120_000;

/// Impressions after which a user's dwell correction counts at half strength.
pub const DWELL_CONFIDENCE_PRIOR: f64 = 25.0;

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

/// Fraction of a correction that `observations` justify: `n / (n + prior)`.
///
/// `0.0` for no observations, approaching `1.0` without reaching it. A prior of
/// `0` or less means "trust everything", which is only correct for signals that are
/// not estimates at all.
#[must_use]
pub fn confidence(observations: u32, prior: f64) -> f64 {
    if prior <= 0.0 {
        return 1.0;
    }
    let n = f64::from(observations);
    n / (n + prior)
}

/// Shrink `value` by the confidence its evidence justifies.
#[must_use]
pub fn shrink(value: f64, observations: u32, prior: f64) -> f64 {
    value * confidence(observations, prior)
}

/// Attention correction for one impression, in label units.
///
/// The baseline is the *user's own* median dwell, not a global threshold, so a
/// person who habitually leaves posts open is not rewarded for it — only their
/// unusually long dwells are. The curve is a saturating logarithm mapped through
/// `tanh`, symmetric around the baseline: faster than usual is mildly negative,
/// slower is mildly positive, and neither grows without bound.
///
/// Returns `0.0` when either input is zero or the ratio is not finite, which is
/// the "not measured" case: the caller had no dwell and must not be penalised for
/// the absence.
#[must_use]
pub fn dwell_correction(dwell_ms: u32, baseline_ms: u32) -> f64 {
    if dwell_ms == 0 || baseline_ms == 0 {
        return 0.0;
    }
    let dwell = f64::from(dwell_ms.min(DWELL_CAP_MS));
    let baseline = f64::from(baseline_ms);
    let ratio = dwell / baseline;
    if !ratio.is_finite() || ratio <= 0.0 {
        return 0.0;
    }
    let raw = DWELL_WEIGHT * (ratio.ln() / 2.0).tanh();
    raw.clamp(-DWELL_MAX_CORRECTION, DWELL_MAX_CORRECTION)
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
/// `0.0` means "no penalty left", `1.0` means "a fresh explicit rejection". Both
/// decays (time) and shrinkage (thin evidence) apply, because one impression must
/// not be able to bury an account on its own.
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
        DISMISS_HALF_LIFE_DAYS, DWELL_MAX_CORRECTION, NOT_INTERESTED_HALF_LIFE_DAYS,
        PENALTY_CONFIDENCE_PRIOR, confidence, decayed_penalty, dwell_correction, penalty_half_life,
        shrink,
    };
    use crate::labels::EventKind;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn confidence_starts_at_zero_and_approaches_one() {
        assert!(approx(confidence(0, 25.0), 0.0));
        assert!(confidence(25, 25.0) < 0.51);
        assert!(confidence(25, 25.0) > 0.49);
        assert!(confidence(2_500, 25.0) > 0.98);
        assert!(confidence(2_500, 25.0) < 1.0);
        // A non-positive prior trusts everything, which is only right for facts.
        assert!(approx(confidence(0, 0.0), 1.0));
    }

    #[test]
    fn shrinkage_keeps_thin_evidence_quiet() {
        let full = 1.0;
        let thin = shrink(full, 2, 25.0);
        let thick = shrink(full, 200, 25.0);
        assert!(
            thin < 0.1,
            "two observations must move almost nothing: {thin}"
        );
        assert!(thick > 0.8, "two hundred must count: {thick}");
        assert!(thin < thick);
    }

    #[test]
    fn dwell_is_symmetric_around_the_baseline() {
        let faster = dwell_correction(1_000, 8_000);
        let slower = dwell_correction(64_000, 8_000);
        assert!(faster < 0.0);
        assert!(slower > 0.0);
        assert!(
            approx(faster, -slower),
            "the curve must be symmetric: {faster} vs {slower}"
        );
        assert!(approx(dwell_correction(8_000, 8_000), 0.0));
    }

    #[test]
    fn dwell_saturates_and_is_capped() {
        let extreme = dwell_correction(u32::MAX, 1);
        assert!(extreme <= DWELL_MAX_CORRECTION);
        assert!(extreme > 0.0);
        // The cap on believable dwell keeps a tab left open from being extreme.
        assert!(approx(
            dwell_correction(u32::MAX, 1),
            dwell_correction(super::DWELL_CAP_MS, 1)
        ));
        let instant = dwell_correction(1, u32::MAX / 2);
        assert!(instant >= -DWELL_MAX_CORRECTION);
    }

    #[test]
    fn an_unmeasured_dwell_is_not_a_signal() {
        assert!(approx(dwell_correction(0, 8_000), 0.0));
        assert!(approx(dwell_correction(8_000, 0), 0.0));
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
        let full = super::normalized_penalty(5.0, 0.0, half_life, 100_000);
        assert!(
            (0.99..=1.0).contains(&full),
            "a fresh rejection is the unit: {full}"
        );
        // Thin evidence keeps the same rejection quiet.
        let thin = super::normalized_penalty(5.0, 0.0, half_life, 1);
        assert!(
            thin < full / 3.0,
            "one impression must not bury an account: {thin}"
        );
        // Nothing recorded is nothing punitive.
        let none = super::normalized_penalty(0.0, 0.0, half_life, 50);
        assert!(approx(none, 0.0));
        // And time alone can take it below any threshold worth acting on.
        let aged = super::normalized_penalty(5.0, 10.0 * half_life, half_life, 100_000);
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
