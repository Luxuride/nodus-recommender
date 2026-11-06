//! Feedback labels for the offline ranker.
//!
//! `docs/06-ranking.md` §Learning from feedback turns interaction events into
//! logistic-regression targets. The values are ordinal, not calibrated: they order
//! outcomes and are only used offline, never as weights on the request path.

/// What the user did with a recommendation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// The item was shown; nothing else was recorded.
    Impression,
    /// The user opened the profile or post.
    Click,
    /// The user followed the account.
    Follow,
    /// The user dismissed the item ("show me less").
    Dismiss,
    /// The user marked the account as not interested.
    NotInterested,
    /// The user boosted the post.
    Boost,
    /// The user replied to the post.
    Reply,
}

/// Ordinal target value for `kind`.
///
/// `+3` follow, `+2` boost or reply, `+1` click, `0` impression, `-2` dismiss,
/// `-5` not interested. The values are ordinal, not calibrated, and are only used
/// as logistic-regression targets in offline training.
#[must_use]
pub fn label(kind: EventKind) -> f64 {
    match kind {
        EventKind::Follow => 3.0,
        EventKind::Boost | EventKind::Reply => 2.0,
        EventKind::Click => 1.0,
        EventKind::Impression => 0.0,
        EventKind::Dismiss => -2.0,
        EventKind::NotInterested => -5.0,
    }
}

#[cfg(test)]
mod tests {
    use super::{EventKind, label};

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn labels_are_ordered_with_one_documented_tie() {
        // `boost` and `reply` share a value: both are a strong, explicit signal.
        let ordered = [
            EventKind::NotInterested,
            EventKind::Dismiss,
            EventKind::Impression,
            EventKind::Click,
            EventKind::Boost,
            EventKind::Follow,
        ];
        for pair in ordered.windows(2) {
            assert!(label(pair[0]) < label(pair[1]));
        }
        assert!(approx(label(EventKind::Reply), label(EventKind::Boost)));
    }

    #[test]
    fn label_values_match_the_document() {
        assert!(approx(label(EventKind::Follow), 3.0));
        assert!(approx(label(EventKind::Boost), 2.0));
        assert!(approx(label(EventKind::Reply), 2.0));
        assert!(approx(label(EventKind::Click), 1.0));
        assert!(approx(label(EventKind::Impression), 0.0));
        assert!(approx(label(EventKind::Dismiss), -2.0));
        assert!(approx(label(EventKind::NotInterested), -5.0));
    }
}
