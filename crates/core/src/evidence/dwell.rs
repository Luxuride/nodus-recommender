//! Attention as evidence, bounded: dwell nudges a label and never reclassifies it.

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

#[cfg(test)]
mod tests {
    use super::{DWELL_CAP_MS, DWELL_MAX_CORRECTION, confidence, dwell_correction, shrink};

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
            dwell_correction(DWELL_CAP_MS, 1)
        ));
        let instant = dwell_correction(1, u32::MAX / 2);
        assert!(instant >= -DWELL_MAX_CORRECTION);
    }

    #[test]
    fn an_unmeasured_dwell_is_not_a_signal() {
        assert!(approx(dwell_correction(0, 8_000), 0.0));
        assert!(approx(dwell_correction(8_000, 0), 0.0));
    }
}
