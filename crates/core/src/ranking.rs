//! Account ranking for the "who to follow" surface.
//!
//! Mirrors `docs/06-ranking.md` §Account ranking: a small linear model over the
//! graph score, support, engagement prior, recency, locale and fatigue terms.
//! Every term is inspectable; the weights are seeded from the document and later
//! replaced by `model_version.weights` rows.
//!
//! Three terms extend the document's formula: taste drift and popularity lift
//! scale the graph term, and a stability bonus rewards an account the user was
//! already shown. All three are inert at their defaults, so the seeded weights
//! keep the documented ranking until the caller starts supplying the signals.

use crate::drift::{Stability, recency_weight, stability_bonus};

/// Relative weight of each ranking term.
///
/// Defaults are the initial values from `docs/06-ranking.md` §Account ranking,
/// plus the drift terms described on [`score`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankingWeights {
    /// Graph evidence, already popularity-damped upstream (default `1.00`).
    pub similarity: f64,
    /// How strongly a stale follow ages out of the graph term (default `0.20`).
    ///
    /// The taste factor is `1 − taste·(1 − recency_weight(follow_age_days))`, so
    /// this is the fraction of the similarity term a maximally stale follow loses.
    /// It is a *rate* of drift, not an independent term: `0.0` freezes taste and
    /// matching `docs/06-ranking.md` exactly.
    pub taste: f64,
    /// Distinct similar users backing the candidate (default `0.35`).
    pub support: f64,
    /// Account-level engagement prior (default `0.25`).
    pub engagement: f64,
    /// Recently active accounts are more likely to be worth a follow (default `0.20`).
    pub freshness: f64,
    /// Mild home-instance preference (default `0.10`).
    pub local: f64,
    /// Language overlap, only when the user's language is known (default `0.10`).
    pub language: f64,
    /// Maximum bonus for a candidate shown on a recent refresh (default `0.15`).
    ///
    /// Bounded by design: an account seen yesterday must not outrank one with
    /// genuinely better evidence, but it should not vanish from the list for a
    /// day either. `0.0` disables the bonus.
    pub stability: f64,
    /// Repeated exposure without interaction (default `0.30`, subtracted).
    pub fatigue: f64,
    /// Rejection strength, `0.0..=1.0` where `1.0` is a fresh explicit rejection
    /// (default `2.00`, subtracted).
    ///
    /// The signal is [`CandidateSignals::penalty`], not a flag: a rejection decays
    /// on its own half-life and is shrunk by how much evidence stands behind it, so
    /// it fades instead of excluding an account forever. `0.0` disables the term.
    pub penalty: f64,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self {
            similarity: 1.00,
            taste: 0.20,
            support: 0.35,
            engagement: 0.25,
            freshness: 0.20,
            local: 0.10,
            language: 0.10,
            stability: 0.15,
            fatigue: 0.30,
            penalty: 2.00,
        }
    }
}

/// One candidate's ranking inputs.
///
/// `affinity` is the graph score normalized to `0..=1`; `support` is the number of
/// distinct similar users who follow the candidate; `age_days` is the age of the
/// candidate's last status; `seen_attention_7d` counts attention spent on this account for
/// this user in the last seven days.
///
/// The drift signals default to "unknown": no follow age, never shown, and a
/// neutral lift of `1.0`. [`Default`] is implemented with exactly those values, so
/// a caller that has none of them yet can spread `..CandidateSignals::default()`
/// over the fields it does have.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CandidateSignals {
    /// Normalized graph affinity for `(user, candidate)`.
    pub affinity: f64,
    /// Number of distinct similar users who follow the candidate.
    pub support: u32,
    /// Account-level engagement prior, popularity-normalized.
    pub engagement_prior: f64,
    /// Days since the candidate last posted.
    pub age_days: f64,
    /// Whether the candidate is on the user's home instance.
    pub same_instance: bool,
    /// Whether the candidate's languages intersect the user's known languages.
    pub language_match: bool,
    /// Impressions of this candidate for this user in the last seven days.
    pub seen_attention_7d: f64,
    /// Rejection strength for this candidate, `0.0..=1.0`.
    ///
    /// Produced by [`crate::evidence::normalized_penalty`]: the stored penalty
    /// decayed by its half-life, shrunk by the number of impressions that back it,
    /// and normalised so `1.0` is a *fully observed* explicit rejection. One
    /// impression is not a pattern, so a single rejection is worth a fraction of the
    /// unit (D27). A candidate with no rejection history is `0.0`, which is also
    /// [`Default`]'s value — absence of evidence is not evidence.
    pub penalty: f64,
    /// Days since the user's follow that contributes to this candidate, if any.
    ///
    /// `None` means the affinity has no per-follow age attached (an aggregate
    /// graph score, or a user with no follow history at all) and the taste factor
    /// is exactly `1.0`.
    pub follow_age_days: Option<f64>,
    /// Whether the candidate was shown to this user on a recent refresh.
    pub stability: Stability,
    /// Ratio of observed to expected follow-through, from
    /// [`crate::damping::popularity_lift`]; `1.0` when unknown.
    pub popularity_lift: f64,
}

impl Default for CandidateSignals {
    /// All signals unknown: no graph evidence, no follow age, never shown, and a
    /// neutral lift. Only `popularity_lift` is non-zero, because `1.0` is the
    /// multiplicative identity the rest of the ranker expects.
    fn default() -> Self {
        Self {
            affinity: 0.0,
            support: 0,
            engagement_prior: 0.0,
            age_days: 0.0,
            same_instance: false,
            language_match: false,
            seen_attention_7d: 0.0,
            penalty: 0.0,
            follow_age_days: None,
            stability: Stability::default(),
            popularity_lift: 1.0,
        }
    }
}

/// Support count at which the support term reaches its full weight.
///
/// `log1p(support) / log1p(50)` in `docs/06-ranking.md` §Account ranking.
pub const SUPPORT_SCALE: f64 = 50.0;

/// Time constant of the freshness decay, in days: `exp(-age_days / 30)`.
const FRESHNESS_TAU_DAYS: f64 = 30.0;

/// Score one candidate, higher is better.
///
/// ```text
/// w.similarity·affinity·taste·lift
///   + w.support·log1p(support)/log1p(50)
///   + w.engagement·engagement_prior
///   + w.freshness·exp(-age_days/30)
///   + w.local·same_instance
///   + w.language·language_match
///   + stability_bonus(stability, w.stability)
///   − w.fatigue·seen_attention_7d
///   − w.penalty·penalty
/// ```
///
/// where `penalty` is a decaying fraction (`0.0..=1.0`) rather than a flag, so a
/// rejection fades by half every seven (`dismiss`) or thirty (`not_interested`)
/// days and can be outvoted by later positive evidence — which requires the explore
/// channel to keep showing such accounts, otherwise nothing can outvote it.
///
/// where `taste = 1 − w.taste·(1 − recency_weight(follow_age_days))` and `taste =
/// 1` when `follow_age_days` is `None`, and `lift = popularity_lift`. The lift
/// multiplies the graph term only: it is the serving-side answer to the
/// exposure-versus-interaction finding described on
/// [`crate::damping::popularity_lift`], so it adjusts how much the graph evidence
/// counts, not how fresh the account is.
///
/// # Compatibility with the pre-drift formula
///
/// With `follow_age_days: None`, `stability.shown_age_days: None` and
/// `popularity_lift: 1.0` — which is what [`CandidateSignals::default`] supplies —
/// the result is exactly the formula documented in `docs/06-ranking.md`: the taste
/// factor is `1.0`, the lift is `1.0`, and the bonus is `0.0`. A follow of age `a`
/// instead shrinks the similarity term by `w.taste·(1 − exp(−a/180))`, at most
/// `w.taste` (20% at the default weights, approached asymptotically, never
/// reached); a lift `l` multiplies that term by `l`; and an account shown `t` days
/// ago gains `w.stability·exp(−t/7)`, at most `w.stability` (`0.15` default).
///
/// So at default weights nothing changes for a caller that supplies no drift
/// signals, and the differences are bounded by the weights when it does: a stale
/// follow can lose at most 20% of its graph evidence, and a recently shown account
/// can gain at most 0.15. Note the ordering consequence — the taste discount applies
/// to `affinity` before `w.similarity`, so at equal raw affinity a fresh follow
/// strictly outranks a stale one, which is the intended change, not an accident.
/// The stability bonus is added after the positive terms and before the fatigue
/// and penalty terms, so those penalties still dominate the list.
#[must_use]
pub fn score(candidate: &CandidateSignals, weights: RankingWeights) -> f64 {
    let affinity = candidate.affinity
        * taste_factor(candidate.follow_age_days, weights.taste)
        * candidate.popularity_lift;
    let mut total = weights.similarity * affinity;
    total += weights.support * f64::from(candidate.support).ln_1p() / SUPPORT_SCALE.ln_1p();
    total += weights.engagement * candidate.engagement_prior;
    total += weights.freshness * (-candidate.age_days / FRESHNESS_TAU_DAYS).exp();
    total += weights.local * flag(candidate.same_instance);
    total += weights.language * flag(candidate.language_match);
    total += stability_bonus(&candidate.stability, weights.stability);
    total -= weights.fatigue * candidate.seen_attention_7d;
    total -= weights.penalty * candidate.penalty;
    total
}

/// `1 − taste·(1 − recency_weight(age))`, or `1.0` when the follow age is unknown.
///
/// A blend rather than a plain product so that `taste` is a drift rate and not a
/// second similarity weight: with `taste = 0.0` the graph term is untouched, and
/// with `taste = 1.0` an arbitrarily old follow contributes nothing. The crate
/// does not clamp weights anywhere else, so a `taste` outside `0..=1` overshoots;
/// the seeded default is `0.20`.
fn taste_factor(follow_age_days: Option<f64>, taste: f64) -> f64 {
    follow_age_days.map_or(1.0, |age| 1.0 - taste * (1.0 - recency_weight(age)))
}

/// `1.0` for `true`, `0.0` for `false`, without a lossy cast.
fn flag(value: bool) -> f64 {
    f64::from(u8::from(value))
}

#[cfg(test)]
mod tests {
    use super::{CandidateSignals, FRESHNESS_TAU_DAYS, RankingWeights, SUPPORT_SCALE, score};
    use crate::drift::{STABILITY_TAU_DAYS, Stability, TASTE_TAU_DAYS, recency_weight};

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn candidate() -> CandidateSignals {
        CandidateSignals {
            affinity: 0.5,
            support: 10,
            engagement_prior: 0.5,
            age_days: 10.0,
            same_instance: false,
            language_match: false,
            seen_attention_7d: 0.0,
            penalty: 0.0,
            follow_age_days: None,
            stability: Stability::default(),
            popularity_lift: 1.0,
        }
    }

    #[test]
    fn stronger_graph_and_freshness_beat_a_distant_candidate() {
        let weights = RankingWeights::default();

        let mut close = candidate();
        close.affinity = 0.9;
        close.support = 40;
        close.age_days = 1.0;

        let mut distant = candidate();
        distant.affinity = 0.2;
        distant.support = 0;
        distant.age_days = 80.0;

        assert!(score(&close, weights) > score(&distant, weights));
    }

    #[test]
    fn a_full_penalty_dominates_otherwise_great_signals() {
        let weights = RankingWeights::default();

        let mut rejected = candidate();
        rejected.affinity = 1.0;
        rejected.support = 50;
        rejected.engagement_prior = 1.0;
        rejected.age_days = 0.0;
        rejected.same_instance = true;
        rejected.language_match = true;
        rejected.penalty = 1.0;

        let mut mediocre = candidate();
        mediocre.affinity = 0.2;
        mediocre.support = 0;
        mediocre.engagement_prior = 0.0;
        mediocre.age_days = 90.0;

        assert!(score(&rejected, weights) < score(&mediocre, weights));
    }

    #[test]
    fn a_decayed_penalty_stops_dominating() {
        let weights = RankingWeights::default();
        let mut faded = candidate();
        faded.affinity = 1.0;
        faded.support = 50;
        faded.penalty = 0.1;

        let mut mediocre = candidate();
        mediocre.affinity = 0.2;
        mediocre.support = 0;
        mediocre.engagement_prior = 0.0;
        mediocre.age_days = 90.0;

        assert!(
            score(&faded, weights) > score(&mediocre, weights),
            "a rejection that has almost decayed must not outrank strong evidence"
        );
    }

    #[test]
    fn repeated_impressions_lower_the_score() {
        let weights = RankingWeights::default();
        let clean = candidate();
        let mut fatigued = candidate();
        fatigued.seen_attention_7d = 3.0;

        assert!(approx(
            score(&clean, weights) - score(&fatigued, weights),
            0.9
        ));
    }

    #[test]
    fn support_term_saturates_at_the_scale() {
        let weights = RankingWeights {
            similarity: 0.0,
            taste: 0.0,
            support: 1.0,
            engagement: 0.0,
            freshness: 0.0,
            local: 0.0,
            language: 0.0,
            stability: 0.0,
            fatigue: 0.0,
            penalty: 0.0,
        };
        let mut backed = candidate();
        backed.support = 50;
        assert!(approx(score(&backed, weights), 1.0));
    }

    #[test]
    fn unknown_drift_signals_reproduce_the_documented_formula() {
        let weights = RankingWeights::default();
        let signals = candidate();
        let expected = {
            let mut total = weights.similarity * signals.affinity;
            total += weights.support * f64::from(signals.support).ln_1p() / SUPPORT_SCALE.ln_1p();
            total += weights.engagement * signals.engagement_prior;
            total += weights.freshness * (-signals.age_days / FRESHNESS_TAU_DAYS).exp();
            total
        };
        assert!(approx(score(&signals, weights), expected));

        // A default-constructed candidate has no graph evidence, so only the
        // freshness term for an age of zero remains.
        assert!(approx(
            score(&CandidateSignals::default(), weights),
            weights.freshness
        ));
    }

    #[test]
    fn a_fresh_follow_outranks_a_stale_one() {
        let weights = RankingWeights::default();

        let mut fresh = candidate();
        fresh.follow_age_days = Some(0.0);

        let mut stale = candidate();
        stale.follow_age_days = Some(TASTE_TAU_DAYS * 3.0);

        assert!(score(&fresh, weights) > score(&stale, weights));

        // The gap is the taste weight times the graph term times the lost decay.
        let expected_gap = weights.similarity
            * fresh.affinity
            * weights.taste
            * (1.0 - recency_weight(TASTE_TAU_DAYS * 3.0));
        assert!(approx(
            score(&fresh, weights) - score(&stale, weights),
            expected_gap
        ));
    }

    #[test]
    fn uptake_lift_raises_a_niche_account_above_an_equally_affine_hub() {
        let weights = RankingWeights::default();

        let mut niche = candidate();
        niche.popularity_lift = 4.0;

        // `candidate()` carries the neutral lift of `1.0`, the hub default.
        let hub = candidate();

        assert!(score(&niche, weights) > score(&hub, weights));
        assert!(approx(
            score(&niche, weights) - score(&hub, weights),
            weights.similarity * hub.affinity * 3.0
        ));
    }

    #[test]
    fn recent_exposure_adds_at_most_the_stability_weight() {
        let weights = RankingWeights::default();

        let novel = candidate();
        let mut shown = candidate();
        shown.stability = Stability {
            shown_age_days: Some(0.0),
            previous_rank: Some(3),
        };

        let bonus = score(&shown, weights) - score(&novel, weights);
        assert!(bonus > 0.0);
        assert!(bonus <= weights.stability + 1e-12);
        assert!(approx(bonus, weights.stability));

        let mut yesterday = candidate();
        yesterday.stability = Stability {
            shown_age_days: Some(STABILITY_TAU_DAYS),
            previous_rank: Some(3),
        };
        let smaller = score(&yesterday, weights) - score(&novel, weights);
        assert!((0.0..bonus).contains(&smaller));
    }
}
