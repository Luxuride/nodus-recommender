//! The wire shape: stored candidates, timestamps and the response envelope.

use nodus_shared::{AccountFeed, AccountItem, CONTRACT_VERSION, FeedAccount};
use nodus_store::feed::Candidate;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// Converts a stored candidate into the wire shape.
pub(super) fn feed_account(candidate: &Candidate) -> FeedAccount {
    FeedAccount {
        id: candidate.account_id.to_string(),
        acct: candidate.acct.clone(),
        display_name: candidate.display_name.clone().unwrap_or_default(),
        note: candidate.note.clone().unwrap_or_default(),
        avatar_url: candidate.avatar_url.clone().unwrap_or_default(),
        followers_count: i64::from(candidate.followers_count),
        last_status_at: candidate.last_status_at.and_then(since_epoch),
        bot: candidate.bot,
        already_following: false,
    }
}

/// Formats a timestamp as RFC 3339, dropping values outside the representable range.
pub(super) fn since_epoch(at: OffsetDateTime) -> Option<String> {
    at.format(&Rfc3339).ok()
}

/// Narrows an hour count to the wire's `f32`.
///
/// The value is an age hint for a banner, not a measurement: a day-boundary
/// difference of one part in ten million is not worth a wider field.
#[expect(
    clippy::cast_possible_truncation,
    reason = "feed metadata is an approximation for a banner; f32 is what the contract carries"
)]
fn hours_to_f32(hours: f64) -> f32 {
    hours as f32
}

/// Assembles the response envelope.
pub(super) fn envelope(
    _sync_enabled: bool,
    age_hours: Option<f64>,
    degraded: Option<String>,
    feed_id: Uuid,
    items: Vec<AccountItem>,
    next_cursor: Option<String>,
) -> AccountFeed {
    AccountFeed {
        feed_id: feed_id.to_string(),
        generated_at: OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_default(),
        contract: CONTRACT_VERSION.to_owned(),
        model_version: 0,
        similarity_age_hours: age_hours.map_or(0.0, hours_to_f32),
        degraded,
        signals_unavailable: Vec::new(),
        items,
        next_cursor,
    }
}
