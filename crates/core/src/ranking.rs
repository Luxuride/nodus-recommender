//! Account ranking for the "who to follow" surface.
//!
//! Mirrors `docs/06-ranking.md` §Account ranking: a small linear model over the
//! graph score, support, engagement prior, recency, locale and fatigue terms.
//! Every term is inspectable; the weights are seeded from the document and later
//! replaced by `model_version.weights` rows.

/// Relative weight of each ranking term.
///
/// Defaults are the initial values from `docs/06-ranking.md` §Account ranking.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankingWeights {
    /// Graph evidence, already popularity-damped upstream (default `1.00`).
    pub similarity: f64,
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
    /// Repeated exposure without interaction (default `0.30`, subtracted).
    pub fatigue: f64,
    /// Explicit "not interested" (default `2.00`, subtracted).
    pub dismissal: f64,
}

impl Default for RankingWeights {
    fn default() -> Self {
        Self {
            similarity: 1.00,
            support: 0.35,
            engagement: 0.25,
            freshness: 0.20,
            local: 0.10,
            language: 0.10,
            fatigue: 0.30,
            dismissal: 2.00,
        }
    }
}

/// One candidate's ranking inputs.
///
/// `affinity` is the graph score normalized to `0..=1`; `support` is the number of
/// distinct similar users who follow the candidate; `age_days` is the age of the
/// candidate's last status; `seen_count_7d` counts impressions of this account for
/// this user in the last seven days.
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
    pub seen_count_7d: u32,
    /// Whether the user dismissed or marked the candidate not interested.
    pub dismissed: bool,
}

/// Support count at which the support term reaches its full weight.
///
/// `log1p(support) / log1p(50)` in `docs/06-ranking.md` §Account ranking.
pub const SUPPORT_SCALE: f64 = 50.0;

/// Time constant of the freshness decay, in days: `exp(-age_days / 30)`.
const FRESHNESS_TAU_DAYS: f64 = 30.0;

/// Score one candidate, higher is better.
///
/// Implements `docs/06-ranking.md` §Account ranking exactly:
/// `w.similarity·affinity + w.support·log1p(support)/log1p(50) +
/// w.engagement·engagement_prior + w.freshness·exp(-age_days/30) +
/// w.local·same_instance + w.language·language_match − w.fatigue·seen_count_7d −
/// w.dismissal·dismissed`.
#[must_use]
pub fn score(candidate: &CandidateSignals, weights: RankingWeights) -> f64 {
    let mut total = weights.similarity * candidate.affinity;
    total += weights.support * f64::from(candidate.support).ln_1p() / SUPPORT_SCALE.ln_1p();
    total += weights.engagement * candidate.engagement_prior;
    total += weights.freshness * (-candidate.age_days / FRESHNESS_TAU_DAYS).exp();
    total += weights.local * flag(candidate.same_instance);
    total += weights.language * flag(candidate.language_match);
    total -= weights.fatigue * f64::from(candidate.seen_count_7d);
    total -= weights.dismissal * flag(candidate.dismissed);
    total
}

/// `1.0` for `true`, `0.0` for `false`, without a lossy cast.
fn flag(value: bool) -> f64 {
    f64::from(u8::from(value))
}

#[cfg(test)]
mod tests {
    use super::{CandidateSignals, RankingWeights, score};

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
            seen_count_7d: 0,
            dismissed: false,
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
    fn dismissal_dominates_otherwise_great_signals() {
        let weights = RankingWeights::default();

        let mut dismissed = candidate();
        dismissed.affinity = 1.0;
        dismissed.support = 50;
        dismissed.engagement_prior = 1.0;
        dismissed.age_days = 0.0;
        dismissed.same_instance = true;
        dismissed.language_match = true;
        dismissed.dismissed = true;

        let mut mediocre = candidate();
        mediocre.affinity = 0.2;
        mediocre.support = 0;
        mediocre.engagement_prior = 0.0;
        mediocre.age_days = 90.0;

        assert!(score(&dismissed, weights) < score(&mediocre, weights));
    }

    #[test]
    fn repeated_impressions_lower_the_score() {
        let weights = RankingWeights::default();
        let clean = candidate();
        let mut fatigued = candidate();
        fatigued.seen_count_7d = 3;

        assert!(approx(
            score(&clean, weights) - score(&fatigued, weights),
            0.9
        ));
    }

    #[test]
    fn support_term_saturates_at_the_scale() {
        let weights = RankingWeights {
            similarity: 0.0,
            support: 1.0,
            engagement: 0.0,
            freshness: 0.0,
            local: 0.0,
            language: 0.0,
            fatigue: 0.0,
            dismissal: 0.0,
        };
        let mut backed = candidate();
        backed.support = 50;
        assert!(approx(score(&backed, weights), 1.0));
    }
}
