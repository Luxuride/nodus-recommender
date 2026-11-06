//! Liveness and readiness.
//!
//! Two endpoints, two different questions:
//!
//! * `/healthz` — is the process alive? It touches nothing, so a database blip does not
//!   cause a restart loop.
//! * `/readyz` — should this instance receive traffic? It checks the one dependency
//!   without which no route can answer: Postgres.

use crate::state::AppState;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// Liveness.
pub async fn healthz() -> &'static str {
    "ok"
}

/// What `/readyz` reports.
#[derive(Debug, Serialize)]
pub struct Readiness {
    /// `ok` or `degraded`.
    pub status: &'static str,
    /// Seconds since process start.
    pub uptime_s: u64,
    /// Whether a trivial query succeeded.
    pub database: bool,
    /// Pool statistics, useful when the service is slow but not failing.
    pub pool_size: u32,
    /// Outstanding jobs by priority, when the database is reachable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backlog: Option<Vec<PriorityBacklog>>,
}

/// One row of the queue snapshot.
#[derive(Debug, Serialize)]
pub struct PriorityBacklog {
    /// Higher urgency is a lower number; P0 is user-facing work.
    pub priority: i16,
    /// Jobs waiting at that priority.
    pub pending: i64,
}

/// Readiness.
pub async fn readyz(State(state): State<AppState>) -> Response {
    let database = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();
    let backlog = if database {
        nodus_store::jobs::backlog_by_priority(&state.pool)
            .await
            .ok()
            .map(|rows| {
                rows.into_iter()
                    .map(|(priority, pending)| PriorityBacklog { priority, pending })
                    .collect()
            })
    } else {
        None
    };
    let status = if database { "ok" } else { "degraded" };
    let body = Readiness {
        status,
        uptime_s: state.uptime_seconds(),
        database,
        pool_size: state.pool.size(),
        backlog,
    };
    let code = if database {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body)).into_response()
}
