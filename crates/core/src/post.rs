//! Post-level ranking for the for-you timeline.
//!
//! `docs/06-ranking.md` §Post ranking: author affinity dominates, then recency
//! decays with an 8-hour half-life, and reply/media/seen factors discount posts
//! that are cheaper to skip.

/// Half-life of the recency decay, in hours: every 8 hours a post's score halves.
///
/// An 8-hour half-life keeps the feed fresh without collapsing it to "everything
/// from the last hour", as `docs/06-ranking.md` explains.
pub const POST_HALF_LIFE_HOURS: f64 = 8.0;

/// Multiplier for a post that replies to an account the user does not follow.
pub const REPLY_PENALTY: f64 = 0.5;

/// Multiplier for a media post the client cannot preload.
///
/// `docs/06-ranking.md` applies it when a post has no text and the client cannot
/// preload media, i.e. a media-only post that reads as a blank row.
pub const MEDIA_PENALTY: f64 = 0.9;

/// Multiplier for a post the user has already been shown.
///
/// v1 collapses the document's `1 − 0.15·min(seen_count, 3)` into a single factor
/// because the caller only knows whether the post was shown, not how often.
pub const SEEN_PENALTY: f64 = 0.6;

/// Hard age cut-off: posts older than this many days are not ranked at all.
pub const POST_MAX_AGE_DAYS: u32 = 7;

/// Score one post, higher is better.
///
/// `affinity · 0.5^(age_hours / 8) · engagement_prior · reply · media · seen`,
/// where each penalty factor applies only when its flag is set and is `1.0`
/// otherwise.
#[must_use]
pub fn post_score(
    affinity: f64,
    age_hours: f64,
    engagement_prior: f64,
    is_reply: bool,
    has_media: bool,
    seen: bool,
) -> f64 {
    let recency = 0.5f64.powf(age_hours / POST_HALF_LIFE_HOURS);
    let reply = if is_reply { REPLY_PENALTY } else { 1.0 };
    let media = if has_media { MEDIA_PENALTY } else { 1.0 };
    let seen_factor = if seen { SEEN_PENALTY } else { 1.0 };
    affinity * recency * engagement_prior * reply * media * seen_factor
}

#[cfg(test)]
mod tests {
    use super::{MEDIA_PENALTY, REPLY_PENALTY, SEEN_PENALTY, post_score};

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn score_halves_at_the_half_life() {
        assert!((post_score(1.0, 8.0, 1.0, false, false, false) - 0.5).abs() < 1e-12);
        assert!((post_score(1.0, 16.0, 1.0, false, false, false) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn penalties_multiply_when_flagged() {
        let base = post_score(1.0, 0.0, 1.0, false, false, false);
        assert!(approx(base, 1.0));
        assert!(approx(
            post_score(1.0, 0.0, 1.0, true, false, false),
            REPLY_PENALTY
        ));
        assert!(approx(
            post_score(1.0, 0.0, 1.0, false, true, false),
            MEDIA_PENALTY
        ));
        assert!(approx(
            post_score(1.0, 0.0, 1.0, false, false, true),
            SEEN_PENALTY
        ));
    }

    #[test]
    fn older_posts_rank_lower() {
        let fresh = post_score(0.8, 1.0, 0.5, false, false, false);
        let stale = post_score(0.8, 48.0, 0.5, false, false, false);
        assert!(fresh > stale);
    }
}
