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
use nodus_shared::AccountFeed;
use nodus_store::feed::{self, Candidate};
use std::time::Duration;
use time::OffsetDateTime;
use uuid::Uuid;

use self::params::{DEFAULT_LIMIT, MAX_LIMIT, MIN_LIMIT, contract_gate, degraded_flag, split};
use self::ranking::is_fresh_rejection;
use self::response::{envelope, snapshot_item};

mod page;
mod params;
pub mod posts;
mod ranking;
pub mod refresh;
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
        ensure_continuable(&state, user.id, feed_id).await?;
        let rows = feed::page(&state.pool, user.id, feed_id, after, limit + 1).await?;
        let (page, more) = split(rows, limit);
        let items = page.into_iter().map(snapshot_item).collect::<Vec<_>>();
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

    // A first page normally means "give me a fresh ranking", but not within the reuse
    // window: two tabs opened a minute apart must show the same accounts, or the user
    // sees one list, clicks a reason, and finds a different list behind it. `refresh`
    // is the caller's way to say it wants a new one anyway.
    if params.refresh != Some(true)
        && let Some(reused) =
            reuse_live_snapshot(&state, user, limit, age, degraded.clone()).await?
    {
        return Ok(reused);
    }

    // First page: rank, record, then answer. `refresh` already short-circuited the
    // reuse window above; everything below is the shared fresh-page path.
    Ok(Json(
        refresh::fresh_page(&state, user, limit, age, degraded).await?,
    ))
}

#[cfg(test)]
mod tests;

/// Drops the rejections the feed must obey before ranking.
///
/// A fresh `not_interested` is the one rejection withheld outright: it is an
/// instruction ("stop"), and the honest way to honour it is to not show the account
/// for a month rather than to show it and score it low. Every other rejection —
/// including a stale `not_interested` — is carried into `score` as a decaying
/// penalty (D26), because an account excluded forever can never be recovered by the
/// positive evidence that follows a changed mind.
fn without_fresh_rejections(candidates: Vec<Candidate>, now: OffsetDateTime) -> Vec<Candidate> {
    candidates
        .into_iter()
        .filter(|candidate| !is_fresh_rejection(candidate, now))
        .collect()
}

/// The low half of a feed's identity, used as the exploration seed.
///
/// The subtlety worth stating: a `UUIDv7` begins with a millisecond timestamp, so the
/// first eight bytes differ per snapshot and *are* deterministic within one. That is
/// exactly the property wanted — two replicas serving the same snapshot pick the same
/// explore slots, while a refresh explores differently.
pub(super) fn seed_of(feed_id: Uuid) -> u64 {
    let bytes = feed_id.as_bytes();
    let mut seed = [0_u8; 8];
    seed.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(seed)
}

/// How long a snapshot may be reused for a first page, and how long it may be continued.
///
/// The two windows are different on purpose. Reuse is a consistency guarantee: every
/// request inside it sees the same list. Continuation is a lifetime guarantee: it
/// only says a reader who started scrolling can finish, because their cursor points
/// into a ranking that still exists.
const SNAPSHOT_REUSE: Duration = Duration::from_secs(10 * 60);

/// The reuse window as a Postgres interval, so the comparison happens in the database.
///
/// SQL rather than Rust because `now()` is the clock the rows were stamped with; a
/// replica's own clock would answer "how old is this" differently on every pod.
fn snapshot_reuse_interval() -> sqlx::postgres::types::PgInterval {
    sqlx::postgres::types::PgInterval {
        months: 0,
        days: 0,
        microseconds: i64::try_from(SNAPSHOT_REUSE.as_micros()).unwrap_or(i64::MAX),
    }
}
const SNAPSHOT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Refuses to continue a snapshot that has outlived [`SNAPSHOT_TTL`].
///
/// `410 Gone` rather than an empty page, because the two are different facts. An
/// empty page would look like "the feed ran out"; the truth is "this cursor can never
/// produce another item", and the client's recovery (start a new feed) is the same in
/// both the expired and the never-existed case — so both answer the same code with
/// different sentences.
pub(super) async fn ensure_continuable(
    state: &AppState,
    user_id: Uuid,
    feed_id: Uuid,
) -> Result<(), ApiError> {
    let age = feed::snapshot_age(&state.pool, user_id, feed_id).await?;
    let limit = SNAPSHOT_TTL.as_secs_f64();
    match age {
        Some(seconds) if seconds <= limit => Ok(()),
        Some(seconds) => Err(ApiError::Gone {
            code: "feed_expired",
            message: format!(
                "this feed snapshot is {:.1} hours old; start a new one",
                seconds / 3600.0
            ),
        }),
        None => Err(ApiError::Gone {
            code: "feed_expired",
            message: "this feed snapshot no longer exists; start a new one".to_owned(),
        }),
    }
}

/// Answers from the snapshot this user is already inside, when one is still fresh.
///
/// Split out of the handler because it is a different request outcome with the same
/// wire shape: nothing is ranked, nothing is written, and the only question is whether
/// the recent past is still allowed to be the present. `refresh=true` skips it.
async fn reuse_live_snapshot(
    state: &AppState,
    user: &nodus_store::models::AppUser,
    limit: i64,
    age: Option<f64>,
    degraded: Option<String>,
) -> Result<Option<Json<AccountFeed>>, ApiError> {
    let Some((feed_id, age_seconds)) =
        feed::live_snapshot(&state.pool, user.id, snapshot_reuse_interval()).await?
    else {
        return Ok(None);
    };
    tracing::debug!(%feed_id, age_seconds, "reusing a live snapshot");
    let rows = feed::page(&state.pool, user.id, feed_id, 0, limit + 1).await?;
    let (page, more) = split(rows, limit);
    let items = page.into_iter().map(snapshot_item).collect::<Vec<_>>();
    let next_cursor =
        more.then(|| cursor::encode(feed_id, items.last().map_or(0, |item| item.item_id)));
    Ok(Some(Json(envelope(
        user.sync_enabled,
        age,
        degraded,
        feed_id,
        items,
        next_cursor,
    ))))
}
