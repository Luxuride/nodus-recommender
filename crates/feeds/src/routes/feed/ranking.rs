//! Ranking: how a stored candidate becomes a score and the reason shown with it.

use nodus_core::drift;
use nodus_core::ranking::{self, CandidateSignals, RankingWeights};
use nodus_shared::Reason;
use nodus_store::feed::Candidate;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::response::since_epoch;

/// Ranks one candidate.
pub(super) fn score(candidate: &Candidate, weights: RankingWeights, home: &str) -> f64 {
    let age_days =
        candidate
            .last_status_at
            .and_then(since_epoch)
            .map_or(f64::from(u32::MAX), |last| {
                let then =
                    OffsetDateTime::parse(&last, &Rfc3339).unwrap_or(OffsetDateTime::UNIX_EPOCH);
                ((OffsetDateTime::now_utc() - then).as_seconds_f64() / 86_400.0).max(0.0)
            });
    ranking::score(
        &CandidateSignals {
            affinity: candidate.affinity,
            support: u32::try_from(candidate.support).unwrap_or(0),
            // M1 has no per-account engagement prior yet; the term is inert until the
            // calculator materialises one.
            engagement_prior: 0.0,
            age_days,
            same_instance: candidate.instance_host == home,
            language_match: false,
            seen_count_7d: u32::try_from(candidate.seen_count_7d).unwrap_or(0),
            dismissed: false,
            // A candidate is by definition not followed yet, so there is no follow
            // edge to age; the taste decay applies to the affinity the calculator
            // already aged, not a second time here.
            follow_age_days: None,
            stability: drift::Stability {
                shown_age_days: candidate.last_shown_at.map(shown_age_days),
                previous_rank: None,
            },
            // Lift needs an expected follow rate per account, which the calculator
            // does not produce yet; 1.0 leaves the term neutral.
            popularity_lift: 1.0,
        },
        weights,
    )
}

/// How long ago an account was last shown to this user, in fractional days.
///
/// A reminder that `served_at` can be in the future after a clock change, and a
/// negative age would turn into a stability bonus larger than the weight allows.
fn shown_age_days(served_at: OffsetDateTime) -> f64 {
    ((OffsetDateTime::now_utc() - served_at).as_seconds_f64() / 86_400.0).max(0.0)
}

/// The reason shown to the user, derived from which sources produced the candidate.
pub(super) fn reason_for(candidate: &Candidate) -> Reason {
    let (code, via) = match candidate
        .sources
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [only] => ((*only).to_owned(), None),
        [first, ..] => ("multiple_signals".to_owned(), Some((*first).to_owned())),
        [] => ("affinity".to_owned(), None),
    };
    Reason {
        code,
        support: u8::try_from(candidate.support).unwrap_or(u8::MAX),
        sample_followers: Vec::new(),
        via: via.or_else(|| candidate.sources.first().cloned()),
    }
}

/// The reason used when a stored one cannot be decoded.
pub(super) fn fallback_reason() -> Reason {
    Reason {
        code: "affinity".to_owned(),
        support: 0,
        sample_followers: Vec::new(),
        via: None,
    }
}
