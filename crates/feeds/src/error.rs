//! The error envelope.
//!
//! Every non-2xx response has the same shape — `{"error": {"code", "message",
//! "retry_after_s"}}` — because the client renders one error path. `code` is the
//! machine-readable part and is part of the wire contract; `message` is for humans and
//! may change. Internal details never cross this boundary: they are logged with the
//! request id and replaced by a generic message.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use nodus_shared::{ApiError as WireError, ApiErrorBody};

/// Everything a handler may fail with.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The request is well-formed but semantically wrong.
    #[error("{message}")]
    BadRequest {
        /// Stable machine-readable code.
        code: &'static str,
        /// Human-readable explanation.
        message: String,
    },
    /// No usable bearer token.
    #[error("authentication required")]
    Unauthorized,
    /// The token is valid but the resource is not ours to show.
    #[error("not found")]
    NotFound,
    /// The resource existed and no longer does: a feed snapshot that has expired.
    #[error("{code}")]
    Gone {
        /// Stable machine-readable code.
        code: &'static str,
        /// Human-readable explanation, including what to do next.
        message: String,
    },
    /// The client speaks a contract this build no longer honours.
    #[error("client contract {client} is older than {min}")]
    ClientTooOld {
        /// Contract version the client announced.
        client: String,
        /// Oldest version this build accepts.
        min: String,
    },
    /// A dependency is temporarily unavailable; retry later.
    #[error("{code} unavailable")]
    Unavailable {
        /// Stable machine-readable code.
        code: &'static str,
        /// Seconds the client should wait before retrying.
        retry_after_s: Option<u32>,
    },
    /// Something failed on our side.
    #[error(transparent)]
    Internal(#[from] anyhow_detail::Detail),
}

/// Wrapper so `Internal` can hold opaque context without leaking it to clients.
pub mod anyhow_detail {
    /// Opaque internal error detail, logged but never serialized.
    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    pub struct Detail(pub String);
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        match error {
            sqlx::Error::RowNotFound => Self::NotFound,
            other => Self::Internal(anyhow_detail::Detail(other.to_string())),
        }
    }
}

impl ApiError {
    /// A request we understood and refuse.
    #[must_use]
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::BadRequest {
            code,
            message: message.into(),
        }
    }

    /// A dependency that is temporarily unable to serve.
    #[must_use]
    pub const fn unavailable(code: &'static str, retry_after_s: u32) -> Self {
        Self::Unavailable {
            code,
            retry_after_s: Some(retry_after_s),
        }
    }

    /// Stable code for the envelope.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest { code, .. }
            | Self::Unavailable { code, .. }
            | Self::Gone { code, .. } => code,
            Self::Unauthorized => "unauthorized",
            Self::NotFound => "not_found",
            Self::ClientTooOld { .. } => "client_too_old",
            Self::Internal(_) => "internal_error",
        }
    }

    /// HTTP status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest { .. } => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Gone { .. } => StatusCode::GONE,
            Self::ClientTooOld { .. } => StatusCode::UPGRADE_REQUIRED,
            Self::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Message shown to the caller.
    #[must_use]
    pub fn client_message(&self) -> String {
        match self {
            Self::BadRequest { message, .. } | Self::Gone { message, .. } => message.clone(),
            Self::Unauthorized => "a Mastodon access token is required".to_owned(),
            Self::NotFound => "no such resource".to_owned(),
            Self::ClientTooOld { client, min } => format!(
                "this client speaks contract {client}, but this server requires at least {min}; \
                 update the client"
            ),
            Self::Unavailable { code, .. } => format!("{code}: try again shortly"),
            // The detail stays in the log; the caller gets a fixed sentence.
            Self::Internal(_) => "internal error".to_owned(),
        }
    }

    /// Seconds the caller should wait, when the error is retryable.
    #[must_use]
    pub fn retry_after_s(&self) -> Option<u32> {
        match self {
            Self::Unavailable { retry_after_s, .. } => *retry_after_s,
            _ => None,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let retry_after = self.retry_after_s();
        if let Self::Internal(detail) = &self {
            tracing::error!(error = %detail, "request failed");
        }
        let body = ApiErrorBody {
            error: WireError {
                code: self.code().to_owned(),
                message: self.client_message(),
                retry_after_s: retry_after,
            },
        };
        let mut response = (status, Json(body)).into_response();
        if let Some(seconds) = retry_after
            && let Ok(value) = axum::http::HeaderValue::from_str(&seconds.to_string())
        {
            response
                .headers_mut()
                .insert(axum::http::header::RETRY_AFTER, value);
        }
        response
    }
}
