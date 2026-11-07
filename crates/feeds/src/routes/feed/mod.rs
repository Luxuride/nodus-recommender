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
//!
//! The directory separates the request from the two things it does: [`params`]
//! parses and guards the request, [`ranking`] turns a stored candidate into a
//! score and a reason, and [`response`] assembles the wire shape. This file keeps
//! the handler that sequences them.

use crate::auth::authenticate;
use crate::cursor;
use crate::error::ApiError;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use nodus_core::ranking::RankingWeights;
use nodus_shared::{AccountFeed, AccountItem, FeedAccount};
use nodus_store::feed::{self, NewItem};
use std::collections::HashMap;
use uuid::Uuid;

use self::params::{DEFAULT_LIMIT, MAX_LIMIT, MIN_LIMIT, contract_gate, degraded_flag, split};
use self::ranking::{fallback_reason, reason_for, score};
use self::response::{envelope, feed_account, since_epoch};

mod params;
mod ranking;
mod response;

pub use params::FeedParams;

/// `GET /api/v1/feed/accounts`
///
/// # Errors
///
/// - [`ApiError::ClientTooOld`] when the caller's contract is below
///   [`nodus_shared::MIN_CONTRACT_VERSION`].
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
    let mut candidates = feed::candidates(&state.pool, user.id, limit + 1).await?;
    if candidates.is_empty() {
        // No affinity has been materialised for this user yet: serve the
        // instance-popular rung instead of an empty first page. One ranking pass
        // covers both, so scores and ordering keep the same shape either way.
        candidates = feed::popular_candidates(&state.pool, user.id, limit + 1).await?;
    }
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

#[cfg(test)]
mod tests;
