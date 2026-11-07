//! Asking for a new snapshot instead of the one you were reading.
//!
//! A first page is normally answered from the live snapshot for ten minutes (see
//! [`super::reuse_live_snapshot`]): two tabs a minute apart must show the same list.
//! A refresh is the caller saying "I know what I was looking at, rank again" — the
//! old snapshot stays continuable until its own lifetime ends, so the reader's scroll
//! position is never invalidated by someone else pressing refresh in another tab.
//!
//! This file owns both halves of "rank a fresh page": the shared ranking/recording
//! body, and the endpoint that names the snapshot being superseded.

use super::page;
use super::params::{
    DEFAULT_LIMIT, FeedParams, MAX_LIMIT, MIN_LIMIT, contract_gate, degraded_flag,
};
use super::ranking::{reason_for, score};
use super::response::{envelope, feed_account};
use super::{ensure_continuable, seed_of, without_fresh_rejections};
use crate::auth::authenticate;
use crate::cursor;
use crate::error::ApiError;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use nodus_core::ranking::RankingWeights;
use nodus_shared::{AccountFeed, AccountItem};
use nodus_store::feed::{self, NewItem};
use nodus_store::models::AppUser;
use std::collections::HashMap;
use time::OffsetDateTime;
use uuid::Uuid;

/// Ranks a new snapshot, records it, and returns it.
///
/// Shared by the first page of [`super::accounts`] and by [`refresh`]: the two differ
/// only in whether the caller was allowed to skip the reuse window.
pub(super) async fn fresh_page(
    state: &AppState,
    user: &AppUser,
    limit: i64,
    age: Option<f64>,
    degraded: Option<String>,
) -> Result<AccountFeed, ApiError> {
    // One query per request: the baseline is a property of the reader, not of the
    // candidate, and every candidate's fatigue is expressed in it. A failure is not
    // worth failing the page for — fatigue then counts plain impressions, which is
    // what it did before attention was measured.
    let baseline = feed::baseline_ms(&state.pool, user.id)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "dwell baseline unavailable; fatigue counts impressions");
            None
        });
    let model_version = feed::active_model_version(&state.pool).await?;
    let now = OffsetDateTime::now_utc();
    let mut candidates = without_fresh_rejections(
        feed::candidates(&state.pool, user.id, limit + 1, baseline).await?,
        now,
    );
    if candidates.is_empty() {
        // No affinity has been materialised for this user yet: serve the
        // instance-popular rung instead of an empty first page. One ranking pass
        // covers both, so scores and ordering keep the same shape either way.
        candidates = feed::popular_candidates(&state.pool, user.id, limit + 1).await?;
    }
    // The page is planned before ranking because exploration is about positions, not
    // scores: the head is whatever the ranking says is best, and the explore slots come
    // from a rank window below it (D26 needs them to keep producing evidence about
    // accounts the ranking has given up on).
    let feed_id = Uuid::now_v7();
    let mut plan = page::plan(
        candidates.len(),
        usize::try_from(limit).unwrap_or(0),
        seed_of(feed_id),
    );
    // One slot goes to an account the ranking has buried but whose rejection has faded:
    // sampling a rank window can never reach those, so without this a decayed penalty
    // would be permanent in practice even though the model says it is not (D26).
    let _ = page::promote_recovery(&mut plan, &candidates, |candidate| {
        recovering(candidate, now)
    });
    let more = plan.more;
    let weights = RankingWeights::default();
    let ranked = page::select(&candidates, &plan)
        .into_iter()
        .map(|candidate| {
            (
                score(candidate, weights, &user.instance_host, now),
                candidate.clone(),
            )
        })
        .collect::<Vec<_>>();

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
    Ok(envelope(
        user.sync_enabled,
        age,
        degraded,
        feed_id,
        items,
        next_cursor,
    ))
}

/// Whether an account has served its penalty and deserves one more look.
///
/// Two conditions, both required: the penalty has decayed below half (so the rejection
/// is old relative to its half-life) and the account has not been shown for a week (so
/// the re-exposure is a real test rather than another impression of a list the user
/// already skipped).
fn recovering(candidate: &nodus_store::feed::Candidate, now: OffsetDateTime) -> bool {
    const FADED: f64 = 0.5;
    const QUIET_DAYS: i64 = 7;

    if candidate.rejected_kind.is_none() {
        return false;
    }
    if super::ranking::penalty(candidate, now) >= FADED {
        return false;
    }
    candidate.last_shown_at.is_none_or(|shown| {
        let quiet = now - shown;
        quiet.whole_days() >= QUIET_DAYS
    })
}

/// `POST /api/v1/feed/{feed_id}/refresh`
///
/// # Errors
///
/// - [`ApiError::ClientTooOld`] when the caller's contract is below
///   [`nodus_shared::MIN_CONTRACT_VERSION`].
/// - [`ApiError::Unauthorized`] when the token or the instance header is missing.
/// - [`ApiError::BadRequest`] when `feed_id` is not a uuid.
/// - [`ApiError::Gone`] when the named snapshot is already expired, which is reported
///   rather than ignored so a client with a stale tab learns why.
/// - [`ApiError::Database`] when a query fails.
pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(feed_id): Path<String>,
    Query(params): Query<FeedParams>,
) -> Result<Json<AccountFeed>, ApiError> {
    contract_gate(&headers)?;
    let credentials = crate::auth::Credentials::from_headers(&headers)?;
    let authenticated = authenticate(&state, credentials).await?;
    let user = &authenticated.user;

    let superseded = Uuid::parse_str(&feed_id).map_err(|_| {
        ApiError::bad_request(
            "invalid_feed_id",
            "feed_id must be the uuid this service issued",
        )
    })?;
    // The snapshot being left behind must belong to this reader and must still be
    // continuable; refreshing a stranger's id would otherwise create a snapshot from
    // someone else's scroll position.
    ensure_continuable(&state, user.id, superseded).await?;

    let limit = params
        .limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(MIN_LIMIT, MAX_LIMIT);
    let age = feed::similarity_age_hours(&state.pool, user.id).await?;
    let degraded = degraded_flag(user.sync_enabled, age);
    let feed = fresh_page(&state, user, limit, age, degraded).await?;
    Ok(Json(feed))
}
