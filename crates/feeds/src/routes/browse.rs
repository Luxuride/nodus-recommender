//! `POST /api/v1/browse` — let a reader's visit ask for the slower crawl.
//!
//! Reading a shared post or profile is the strongest signal that its author and
//! conversation matter. The response is a receipt, not the data: the client already
//! has the page it asked for, and the crawl continues in the calculator.

use axum::Json;
use axum::extract::State;
use nodus_store::jobs::{self, NewJob};
use nodus_store::models::NewAccount;
use nodus_store::{accounts, users};
use serde::Deserialize;

use crate::error::ApiError;
use crate::state::AppState;

/// What the reader opened.
#[derive(Debug, Deserialize)]
pub struct Browse {
    /// `status` or `profile`.
    pub kind: String,
    /// The instance that owns the id.
    pub host: String,
    /// The instance-local account or status id.
    pub id: String,
    /// The account handle, for a profile the recommender has not met.
    pub acct: Option<String>,
}

/// The number of jobs this visit created.
#[derive(Debug, serde::Serialize)]
pub struct Receipt {
    /// Jobs enqueued, after deduplication.
    pub queued: usize,
}

/// Record a visit as crawl work.
///
/// # Errors
///
/// [`ApiError::BadRequest`] when the target is malformed; otherwise database failures.
pub async fn browse(
    State(state): State<AppState>,
    Json(body): Json<Browse>,
) -> Result<Json<Receipt>, ApiError> {
    let id = body
        .id
        .trim()
        .parse::<i64>()
        .map_err(|error| ApiError::bad_request("bad_target", format!("`{}` is not an id: {error}", body.id)))?;
    if body.host.trim().is_empty() {
        return Err(ApiError::bad_request("bad_target", "the host is empty"));
    }
    let host = body.host.trim().to_owned();
    users::ensure_instance(&state.pool, &host).await?;

    let account_id = match body.kind.as_str() {
        "profile" => Some(
            ensure_account(&state, &host, id, body.acct.as_deref()).await?,
        ),
        "status" => {
            let found: Option<i64> = sqlx::query_scalar(
                "SELECT account_id FROM status WHERE instance_host = $1 AND mastodon_id = $2",
            )
            .bind(&host)
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;
            found
        }
        other => {
            return Err(ApiError::bad_request(
                "bad_kind",
                format!("`{other}` is not `status` or `profile`"),
            ));
        }
    };

    let mut queued = 0;
    if let Some(account_id) = account_id {
        let subject = account_id.to_string();
        for (kind, priority) in [("refresh_account", 2), ("refresh_profile", 3)] {
            if jobs::enqueue(
                &state.pool,
                &NewJob::pending(kind, priority, "account", Some(subject.clone())),
            )
            .await?
            .is_some()
            {
                queued += 1;
            }
        }
    }
    Ok(Json(Receipt { queued }))
}

async fn ensure_account(
    state: &AppState,
    host: &str,
    id: i64,
    acct: Option<&str>,
) -> Result<i64, ApiError> {
    if let Some(found) = accounts::find_id(&state.pool, host, id).await? {
        return Ok(found);
    }
    let Some(acct) = acct.filter(|acct| !acct.trim().is_empty()) else {
        return Err(ApiError::bad_request(
            "unknown_account",
            "a profile the recommender has not met needs its acct",
        ));
    };
    let acct = if acct.contains('@') {
        acct.to_owned()
    } else {
        format!("{acct}@{host}")
    };
    accounts::upsert_many(
        &state.pool,
        &[NewAccount {
            instance_host: host.to_owned(),
            mastodon_id: id,
            remote_id: Some(id.to_string()),
            acct,
            display_name: None,
            followers_count: 0,
            bot: false,
            locked: false,
            discoverable: None,
            indexable: None,
            noindex: None,
        }],
    )
    .await?;
    accounts::find_id(&state.pool, host, id)
        .await?
        .ok_or(ApiError::NotFound)
}
