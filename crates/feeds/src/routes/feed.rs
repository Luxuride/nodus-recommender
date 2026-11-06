//! The account feed.
//!
//! One request does two things that look similar and are not: it **ranks** the
//! candidates the calculator materialised, and it **records** what it served. The
//! recording is not telemetry — an impression has to name an item that existed when the
//! user saw it, so the ids are minted here, in the only table the feed writes.
//!
//! The second page of a feed does neither. It reads rows back by cursor, because a
//! reader who scrolls past item 50 should see item 51 of the list they started, not
//! item 51 of a list re-ranked a second later.

use crate::auth::authenticate;
use crate::cursor;
use crate::error::ApiError;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use nodus_core::ranking::{self, CandidateSignals, RankingWeights};
use nodus_shared::{
    AccountFeed, AccountItem, CONTRACT_VERSION, FeedAccount, MIN_CONTRACT_VERSION, Reason,
};
use nodus_store::feed::{self, Candidate, NewItem};
use serde::Deserialize;
use std::collections::HashMap;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// Query parameters of a feed request.
#[derive(Debug, Deserialize)]
pub struct FeedParams {
    /// Page size; clamped to [`MIN_LIMIT`]`..=`[`MAX_LIMIT`].
    limit: Option<i64>,
    /// Cursor returned by the previous page.
    cursor: Option<String>,
}

/// Smallest page the endpoint will serve.
const MIN_LIMIT: i64 = 1;
/// Largest page the endpoint will serve.
const MAX_LIMIT: i64 = 100;
/// Page size when the client does not ask for one.
const DEFAULT_LIMIT: i64 = 50;

/// `GET /api/v1/feed/accounts`
///
/// # Errors
///
/// - [`ApiError::ClientTooOld`] when the caller's contract is below
///   [`MIN_CONTRACT_VERSION`].
/// - [`ApiError::Unauthorized`] when the token or the instance header is missing or
///   the instance rejects the token.
/// - [`ApiError::BadRequest`] for a cursor this service did not mint.
/// - [`ApiError::Database`] when the query fails.
pub async fn accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<FeedParams>,
) -> Result<Json<AccountFeed>, ApiError> {
    contract_gate(&headers)?;
    let credentials = crate::auth::Credentials::from_headers(&headers)?;
    let authenticated = authenticate(&state, credentials).await?;
    let user = &authenticated.user;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(MIN_LIMIT, MAX_LIMIT);

    let age = feed::similarity_age_hours(&state.pool, user.id).await?;
    let degraded = degraded_flag(user.sync_enabled, age);

    if let Some(cursor) = params.cursor.as_deref() {
        let (feed_id, after) = cursor::decode(cursor).ok_or_else(|| {
            ApiError::bad_request("invalid_cursor", "cursor was not issued by this service")
        })?;
        let rows = feed::page(&state.pool, user.id, feed_id, after, limit + 1).await?;
        let (page, more) = split(rows, limit);
        let items = page
            .into_iter()
            .map(|row| AccountItem {
                item_id: row.id,
                account: FeedAccount {
                    id: row.account_id.to_string(),
                    acct: row.acct,
                    display_name: row.display_name.unwrap_or_default(),
                    note: row.note.unwrap_or_default(),
                    avatar_url: row.avatar_url.unwrap_or_default(),
                    followers_count: i64::from(row.followers_count),
                    last_status_at: row.last_status_at.and_then(since_epoch),
                    bot: row.bot,
                    already_following: false,
                },
                reason: serde_json::from_value(row.reason.clone()).unwrap_or_else(|error| {
                    tracing::warn!(item_id = row.id, %error, "stored reason did not decode");
                    fallback_reason()
                }),
            })
            .collect::<Vec<_>>();
        let next_cursor =
            more.then(|| cursor::encode(feed_id, items.last().map_or(after, |item| item.item_id)));
        return Ok(Json(envelope(
            user.sync_enabled,
            age,
            degraded,
            feed_id,
            items,
            next_cursor,
        )));
    }

    // First page: rank, record, then answer.
    let model_version = feed::active_model_version(&state.pool).await?;
    let candidates = feed::candidates(&state.pool, user.id, limit + 1).await?;
    let (candidates, more) = split(candidates, limit);
    let weights = RankingWeights::default();
    let ranked = candidates
        .into_iter()
        .map(|candidate| (score(&candidate, weights, &user.instance_host), candidate))
        .collect::<Vec<_>>();

    let feed_id = Uuid::now_v7();
    let new_items = ranked
        .iter()
        .map(|(value, candidate)| NewItem {
            account_id: candidate.account_id,
            score: *value,
            reason: serde_json::to_value(reason_for(candidate)).unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    let persisted = feed::persist(&state.pool, user.id, feed_id, model_version, &new_items).await?;
    let by_account: HashMap<i64, i64> = persisted
        .into_iter()
        .map(|(id, account)| (account, id))
        .collect();

    let items = ranked
        .into_iter()
        .filter_map(|(_, candidate)| {
            let item_id = *by_account.get(&candidate.account_id)?;
            Some(AccountItem {
                item_id,
                account: feed_account(&candidate),
                reason: reason_for(&candidate),
            })
        })
        .collect::<Vec<_>>();
    let next_cursor =
        more.then(|| cursor::encode(feed_id, items.last().map_or(0, |item| item.item_id)));
    Ok(Json(envelope(
        user.sync_enabled,
        age,
        degraded,
        feed_id,
        items,
        next_cursor,
    )))
}

/// Rejects a client whose contract version predates ours.
fn contract_gate(headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(client) = headers
        .get("x-contract-version")
        .and_then(|value| value.to_str().ok())
    else {
        // A missing header is allowed: curl and the first integration tests do not send
        // one. A header we cannot understand is not.
        return Ok(());
    };
    let client = client.trim();
    if parse_version(client) < parse_version(MIN_CONTRACT_VERSION) {
        return Err(ApiError::ClientTooOld {
            client: client.to_owned(),
            min: MIN_CONTRACT_VERSION.to_owned(),
        });
    }
    Ok(())
}

/// Parses `major.minor.patch`, ignoring anything after the patch number.
fn parse_version(text: &str) -> (u32, u32, u32) {
    let mut parts = text.split('.').map(|part| {
        part.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    });
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// Ranks one candidate.
fn score(candidate: &Candidate, weights: RankingWeights, home: &str) -> f64 {
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
            seen_count_7d: 0,
            dismissed: false,
        },
        weights,
    )
}

/// The reason shown to the user, derived from which sources produced the candidate.
fn reason_for(candidate: &Candidate) -> Reason {
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
fn fallback_reason() -> Reason {
    Reason {
        code: "affinity".to_owned(),
        support: 0,
        sample_followers: Vec::new(),
        via: None,
    }
}

/// Converts a stored candidate into the wire shape.
fn feed_account(candidate: &Candidate) -> FeedAccount {
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
fn since_epoch(at: OffsetDateTime) -> Option<String> {
    at.format(&Rfc3339).ok()
}

/// Why the feed is not fully personalised, if it is not.
fn degraded_flag(sync_enabled: bool, age_hours: Option<f64>) -> Option<String> {
    if !sync_enabled {
        return Some("no_server_sync".to_owned());
    }
    match age_hours {
        None => Some("cold_start".to_owned()),
        Some(hours) if hours > 48.0 => Some("stale_similarity".to_owned()),
        Some(_) => None,
    }
}

/// Splits a `limit + 1` row read into a page and "there is more".
fn split<T>(mut rows: Vec<T>, limit: i64) -> (Vec<T>, bool) {
    let more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
    if more {
        rows.truncate(usize::try_from(limit).unwrap_or(0));
    }
    (rows, more)
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
fn envelope(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(parse_version("0.2.0") > parse_version("0.1.9"));
        assert!(parse_version("1.0.0") > parse_version("0.99.99"));
        assert_eq!(parse_version("0"), (0, 0, 0));
        assert_eq!(parse_version("0.1.0-rc1"), (0, 1, 0));
    }

    #[test]
    fn a_missing_header_passes_the_gate() {
        assert!(contract_gate(&HeaderMap::new()).is_ok());
    }

    #[test]
    fn an_older_client_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert("x-contract-version", "0.0.1".parse().unwrap());
        assert!(matches!(
            contract_gate(&headers),
            Err(ApiError::ClientTooOld { .. })
        ));
    }

    #[test]
    fn a_current_client_passes() {
        let mut headers = HeaderMap::new();
        headers.insert("x-contract-version", MIN_CONTRACT_VERSION.parse().unwrap());
        assert!(contract_gate(&headers).is_ok());
    }

    #[test]
    fn a_limit_plus_one_read_reports_more() {
        let (page, more) = split(vec![1, 2, 3], 2);
        assert_eq!(page, vec![1, 2]);
        assert!(more);
        let (page, more) = split(vec![1, 2], 2);
        assert_eq!(page, vec![1, 2]);
        assert!(!more);
    }

    #[test]
    fn degradation_prefers_the_cause_over_the_symptom() {
        assert_eq!(
            degraded_flag(false, None).as_deref(),
            Some("no_server_sync")
        );
        assert_eq!(degraded_flag(true, None).as_deref(), Some("cold_start"));
        assert_eq!(
            degraded_flag(true, Some(72.0)).as_deref(),
            Some("stale_similarity")
        );
        assert_eq!(degraded_flag(true, Some(2.0)), None);
    }
}
