//! `GET /api/v1/feed/posts`: what to read, rather than who to follow.
//!
//! The account feed and this one share a snapshot mechanism and nothing else. A post
//! snapshot is never *reused* the way an account snapshot is, because the plausible
//! question behind a reload here is "is there something newer", and the answer to that
//! has to come from the statuses the calculator already stored. Continuation still
//! works: a cursor that points into a snapshot keeps producing that snapshot's items.

use crate::auth::{Credentials, authenticate};
use crate::cursor;
use crate::error::ApiError;
use crate::state::AppState;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use nodus_core::post::post_score;
use nodus_shared::{PostFeed, PostItem, Reason};
use nodus_store::feed::{self, posts::NewPostItem};
use time::OffsetDateTime;
use uuid::Uuid;

use super::ensure_continuable;
use super::params::{DEFAULT_LIMIT, MAX_LIMIT, MIN_LIMIT, contract_gate, degraded_flag};

/// How many ranked candidates a fresh snapshot keeps.
///
/// The wire page is smaller; the rest stay in `recommendation_item` so a cursor can keep
/// reading the same order without asking the ranker to invent a second, inconsistent one.
const SNAPSHOT_LIMIT: usize = 200;

/// `GET /api/v1/feed/posts`
///
/// # Errors
///
/// - [`ApiError::ClientTooOld`] when the caller's contract is below the minimum.
/// - [`ApiError::Unauthorized`] when the token or instance header is missing.
/// - [`ApiError::BadRequest`] for a cursor this service did not mint.
/// - [`ApiError::Gone`] for a cursor older than the snapshot lifetime.
/// - [`ApiError::Database`] when a query fails.
pub async fn posts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<super::FeedParams>,
) -> Result<Json<PostFeed>, ApiError> {
    contract_gate(&headers)?;
    let credentials = Credentials::from_headers(&headers)?;
    let authenticated = authenticate(&state, credentials).await?;
    let user = &authenticated.user;
    let limit = params
        .limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(MIN_LIMIT, MAX_LIMIT);
    let degraded = degraded_flag(user.sync_enabled, None);

    if let Some(cursor) = params.cursor.as_deref() {
        let (feed_id, after) = cursor::decode(cursor).ok_or_else(|| {
            ApiError::bad_request("invalid_cursor", "cursor was not issued by this service")
        })?;
        ensure_continuable(&state, user.id, feed_id).await?;
        let rows = feed::posts::page(&state.pool, user.id, feed_id, after, limit + 1).await?;
        let keep = usize::try_from(limit).unwrap_or(0);
        let more = rows.len() > keep;
        let items = rows
            .into_iter()
            .take(keep)
            .map(post_item)
            .collect::<Vec<_>>();
        let next_cursor =
            more.then(|| cursor::encode(feed_id, items.last().map_or(after, |item| item.item_id)));
        return Ok(Json(PostFeed {
            feed_id: feed_id.to_string(),
            generated_at: rfc3339(OffsetDateTime::now_utc()),
            contract: nodus_shared::CONTRACT_VERSION.to_owned(),
            model_version: feed::active_model_version(&state.pool).await?,
            degraded,
            items,
            next_cursor,
        }));
    }

    let model_version = feed::active_model_version(&state.pool).await?;
    let feed_id = Uuid::now_v7();
    let now = OffsetDateTime::now_utc();
    let mut candidates = feed::posts::candidates(&state.pool, user.id, SNAPSHOT_LIMIT as i64 + 1)
        .await
        .map_err(|error| {
            tracing::error!(?error, "post candidates");
            error
        })?;
    let cold_start = candidates.is_empty();
    if cold_start {
        candidates =
            feed::posts::fallback_candidates(&state.pool, user.id, SNAPSHOT_LIMIT as i64 + 1)
                .await
                .map_err(|error| {
                    tracing::error!(?error, "fallback post candidates");
                    error
                })?;
    }
    let (ranked, _) = rank(candidates, now, SNAPSHOT_LIMIT, cold_start);

    let new_items = ranked
        .iter()
        .map(|(score, candidate)| NewPostItem {
            status_id: candidate.status_id,
            score: *score,
            reason: serde_json::to_value(reason_for()).unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    let persisted =
        feed::posts::persist(&state.pool, user.id, feed_id, model_version, &new_items).await?;

    let all_items = persisted
        .into_iter()
        .zip(ranked.iter())
        .map(|((item_id, _), (_, candidate))| PostItem {
            item_id,
            status_id: candidate.status_id.to_string(),
            account_acct: candidate.account_acct.clone(),
            account_display_name: candidate
                .account_display_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| candidate.account_acct.clone()),
            account_avatar_url: candidate.account_avatar_url.clone().unwrap_or_default(),
            text: candidate.content.clone(),
            published_at: rfc3339(candidate.published_at),
            favourites_count: i64::from(candidate.favourites_count),
            reblogs_count: i64::from(candidate.reblogs_count),
            replies_count: i64::from(candidate.replies_count),
            instance_host: candidate.instance_host.clone(),
            reason: reason_for(),
        })
        .collect::<Vec<_>>();
    let keep = usize::try_from(limit).unwrap_or(0);
    let more = all_items.len() > keep;
    let items = all_items.into_iter().take(keep).collect::<Vec<_>>();
    let next_cursor =
        more.then(|| cursor::encode(feed_id, items.last().map_or(0, |item| item.item_id)));

    tracing::debug!(%feed_id, items = items.len(), snapshot = ranked.len(), "ranked a post snapshot");
    Ok(Json(PostFeed {
        feed_id: feed_id.to_string(),
        generated_at: rfc3339(now),
        contract: nodus_shared::CONTRACT_VERSION.to_owned(),
        model_version,
        degraded,
        items,
        next_cursor,
    }))
}

/// Scores and orders the candidates, then keeps one page.
///
/// Rank first, truncate second: a candidate the scorer rejects (an ancient post with
/// no engagement) must not consume a slot a better one could take. The tie-break on
/// `status_id` keeps two equal scores from swapping places between identical requests,
/// which matters because the page is persisted and read back by id.
fn rank(
    candidates: Vec<feed::posts::PostCandidate>,
    now: OffsetDateTime,
    keep: usize,
    cold_start: bool,
) -> (Vec<(f64, feed::posts::PostCandidate)>, bool) {
    let mut ranked = candidates
        .into_iter()
        .map(|candidate| {
            let age = hours_between(now, candidate.published_at);
            // A cold pool intentionally reaches further back; without a floor the
            // eight-hour half-life turns a year-old post into f64 noise that cannot
            // survive the `real` score column.
            let age = if cold_start { age.min(72.0) } else { age };
            let score = post_score(
                candidate.affinity,
                age,
                engagement_prior(candidate.favourites_count, candidate.reblogs_count),
                candidate.is_reply,
                false,
                false,
            );
            (score, candidate)
        })
        .filter(|(score, _)| score.is_finite() && *score > f64::from(f32::MIN_POSITIVE))
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.status_id.cmp(&b.1.status_id))
    });
    let more = ranked.len() > keep;
    ranked.truncate(keep);
    (ranked, more)
}

/// Why this post, in the same vocabulary the account feed uses.
///
/// One code today: the author earned affinity, and this is what they wrote. A second
/// code would need a second source of post candidates, and inventing it before then
/// would put a reason on the card that no producer stands behind.
fn reason_for() -> Reason {
    Reason {
        code: "author_you_should_follow".to_owned(),
        support: 0,
        sample_followers: Vec::new(),
        via: None,
    }
}

/// Turns a stored snapshot row into the wire shape.
fn post_item(row: feed::posts::SnapshotPost) -> PostItem {
    let reason = serde_json::from_value(row.reason).unwrap_or_else(|_| reason_for());
    PostItem {
        item_id: row.id,
        status_id: row.status_id.to_string(),
        account_acct: row.account_acct.clone(),
        account_display_name: row
            .account_display_name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| row.account_acct.clone()),
        account_avatar_url: row.account_avatar_url.unwrap_or_default(),
        text: row.content,
        published_at: rfc3339(row.published_at),
        favourites_count: i64::from(row.favourites_count),
        reblogs_count: i64::from(row.reblogs_count),
        replies_count: i64::from(row.replies_count),
        instance_host: row.instance_host,
        reason,
    }
}

/// Age of a post in hours, never negative.
///
/// A status stamped in the future (clock skew on a remote instance) must not earn a
/// recency bonus above one, so the value floors at zero.
fn hours_between(now: OffsetDateTime, then: OffsetDateTime) -> f64 {
    ((now - then).as_seconds_f64() / 3600.0).max(0.0)
}

/// How much attention a post got, on a scale that barely grows.
///
/// A boost counts double: it is a public endorsement, while a favourite is a private
/// one. The denominator is `ln(101)` so a post with a hundred favourite-equivalents
/// scores about 2.0 -- large numbers must not be able to outweigh affinity entirely.
fn engagement_prior(favourites: i32, reblogs: i32) -> f64 {
    let weighted = 1.0 + f64::from(favourites) + 2.0 * f64::from(reblogs);
    1.0 + weighted.ln() / 101.0_f64.ln()
}

/// RFC 3339 with `Z`, the only timestamp shape the contract allows.
fn rfc3339(timestamp: OffsetDateTime) -> String {
    timestamp
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
