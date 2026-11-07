//! Ranking: how a stored candidate becomes a score and the reason shown with it.

use nodus_core::damping;
use nodus_core::drift;
use nodus_core::evidence::{
    NOT_INTERESTED_HALF_LIFE_DAYS, base_penalty, normalized_penalty, penalty_half_life,
};
use nodus_core::labels::EventKind;
use nodus_core::ranking::{self, CandidateSignals, RankingWeights};
use nodus_shared::Reason;
use nodus_store::feed::Candidate;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::response::since_epoch;

/// Ranks one candidate as of `now`.
///
/// `now` is threaded in rather than read here so the skip decision and the penalty
/// applied to the same candidate cannot disagree across a clock tick.
pub(super) fn score(
    candidate: &Candidate,
    weights: RankingWeights,
    home: &str,
    now: OffsetDateTime,
) -> f64 {
    let age_days =
        candidate
            .last_status_at
            .and_then(since_epoch)
            .map_or(f64::from(u32::MAX), |last| {
                let then =
                    OffsetDateTime::parse(&last, &Rfc3339).unwrap_or(OffsetDateTime::UNIX_EPOCH);
                days_between(now, then)
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
            penalty: penalty(candidate, now),
            // A candidate is by definition not followed yet, so there is no follow
            // edge to age; the taste decay applies to the affinity the calculator
            // already aged, not a second time here.
            follow_age_days: None,
            stability: drift::Stability {
                shown_age_days: candidate
                    .last_shown_at
                    .map(|served_at| days_between(now, served_at)),
                previous_rank: None,
            },
            // Lift needs an expected follow rate per account, which the calculator
            // does not produce yet; 1.0 leaves the term neutral.
            popularity_lift: popularity_lift(candidate),
        },
        weights,
    )
}

/// How much the instance's own history moves this candidate's affinity (D29).
///
/// `1.0` until the counters exist *and* the account has been shown enough times to
/// mean something: the lift is evidence about this recommendation, not a prior about
/// the account, so thin evidence must leave the term neutral. The two rates are
/// clamped by [`damping::popularity_lift`], which is where the floor and ceiling on
/// a noisy ratio live.
fn popularity_lift(candidate: &Candidate) -> f64 {
    let (Some(shown), Some(followed), Some(expected)) = (
        candidate.follow_through_shown,
        candidate.follow_through_followed,
        candidate.expected_follow_rate,
    ) else {
        return 1.0;
    };
    if shown < MIN_COUNTER_IMPRESSIONS || expected <= 0.0 {
        return 1.0;
    }
    let observed = f64::from(followed) / f64::from(shown);
    damping::popularity_lift(1.0, observed, expected)
}

/// Fewest impressions before the counters can move a score.
///
/// Twenty is roughly one follow-through event at a one-in-twenty baseline: below it,
/// a single follow would read as a doubling, which is noise dressed as evidence.
const MIN_COUNTER_IMPRESSIONS: i32 = 20;

/// How strongly the stored rejection for `candidate` still counts at `now`.
///
/// `0.0` for no rejection and for a kind this build does not recognise: `kind` is
/// stored as text, and a value written by a newer producer must not be read as
/// evidence against an account.
pub(super) fn penalty(candidate: &Candidate, now: OffsetDateTime) -> f64 {
    let Some(stored) = candidate.rejected_kind.as_deref() else {
        return 0.0;
    };
    let kind = match stored {
        "dismiss" => EventKind::Dismiss,
        "not_interested" => EventKind::NotInterested,
        _ => return 0.0,
    };
    let age_days = candidate
        .rejected_at
        .map_or(0.0, |recorded| days_between(now, recorded));
    normalized_penalty(
        base_penalty(kind),
        age_days,
        penalty_half_life(kind).unwrap_or(NOT_INTERESTED_HALF_LIFE_DAYS),
        u32::try_from(candidate.impressions).unwrap_or(0),
    )
}

/// Whether the rejection is a fresh explicit instruction the feed must obey.
///
/// Only `not_interested` inside its first half-life is a hard skip: it is the one
/// rejection stated as "stop", so holding it for a month is what the user asked
/// for. A `dismiss` is a nudge ("show me less"), and a stale `not_interested` has
/// decayed far enough that later positive evidence can outvote it. Both stay in the
/// list with a penalty, because a permanent exclusion is what D26 rejects.
pub(super) fn is_fresh_rejection(candidate: &Candidate, now: OffsetDateTime) -> bool {
    candidate.rejected_kind.as_deref() == Some("not_interested")
        && candidate
            .rejected_at
            .is_some_and(|recorded| days_between(now, recorded) < NOT_INTERESTED_HALF_LIFE_DAYS)
}

/// Fractional days from `then` to `now`, never negative.
///
/// A stored timestamp can be in the future after a clock change, and a negative age
/// would turn into a stability bonus larger than the weight allows.
fn days_between(now: OffsetDateTime, then: OffsetDateTime) -> f64 {
    ((now - then).as_seconds_f64() / 86_400.0).max(0.0)
}

/// The reason shown to the user, derived from which sources produced the candidate.
///
/// A candidate with no sources came from the instance-popular rung served on cold
/// start, not from an affinity calculation, so it is named for what it is instead of
/// claiming a signal the user never provided.
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
        [] => ("popular_fallback".to_owned(), None),
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

#[cfg(test)]
mod tests {
    use super::{is_fresh_rejection, penalty};
    use nodus_store::feed::Candidate;
    use time::{Duration, OffsetDateTime};

    fn candidate(rejected: Option<(&str, i64)>, impressions: i32) -> Candidate {
        let now = OffsetDateTime::now_utc();
        Candidate {
            account_id: 1,
            affinity: 0.5,
            support: 1,
            sources: vec!["similar_user".to_owned()],
            computed_at: now,
            last_shown_at: None,
            seen_count_7d: 0,
            rejected_kind: rejected.map(|(kind, _)| kind.to_owned()),
            rejected_at: rejected.map(|(_, age_days)| now - Duration::days(age_days)),
            impressions,
            follow_through_shown: None,
            follow_through_followed: None,
            expected_follow_rate: None,
            acct: "alice@remote.test".to_owned(),
            display_name: None,
            note: None,
            avatar_url: None,
            followers_count: 10,
            bot: false,
            last_status_at: None,
            instance_host: "remote.test".to_owned(),
        }
    }

    #[test]
    fn only_a_fresh_not_interested_is_withheld() {
        let now = OffsetDateTime::now_utc();
        assert!(is_fresh_rejection(
            &candidate(Some(("not_interested", 3)), 5),
            now
        ));
        assert!(!is_fresh_rejection(
            &candidate(Some(("not_interested", 45)), 5),
            now
        ));
        // A dismiss is a nudge, not an instruction, however recent.
        assert!(!is_fresh_rejection(
            &candidate(Some(("dismiss", 1)), 5),
            now
        ));
        assert!(!is_fresh_rejection(&candidate(None, 5), now));
    }

    #[test]
    fn a_rejection_becomes_a_decaying_penalty() {
        let now = OffsetDateTime::now_utc();
        assert!(penalty(&candidate(None, 10), now) < f64::EPSILON);
        // A kind stored by a newer producer is not evidence this build can read.
        assert!(penalty(&candidate(Some(("muted", 0)), 10), now) < f64::EPSILON);
        let fresh = penalty(&candidate(Some(("not_interested", 0)), 5), now);
        let aged = penalty(&candidate(Some(("not_interested", 60)), 5), now);
        assert!(fresh > aged);
        assert!(aged > 0.0);
    }
}
