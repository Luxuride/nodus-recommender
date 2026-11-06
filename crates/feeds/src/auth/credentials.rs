//! What a request presents: the bearer token and the claimed instance.

use axum::http::{HeaderMap, header};

use super::headers::{INSTANCE_HEADER, is_plausible_host};
use crate::error::ApiError;

/// A bearer token, kept from `Debug` output.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Wrap a raw token.
    #[must_use]
    pub fn new(raw: String) -> Self {
        Self(raw)
    }

    /// The token, for the one place that has to send it onwards.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// What a request claims about its caller.
#[derive(Debug, Clone)]
pub struct Credentials {
    /// Instance the token belongs to, lower-cased, without a scheme.
    pub instance_host: String,
    /// The presented bearer token.
    pub token: Token,
}

impl Credentials {
    /// Read credentials from request headers.
    ///
    /// # Errors
    ///
    /// [`ApiError::BadRequest`] with `missing_credentials` when the `Authorization`
    /// header is absent or not a bearer token, `missing_instance_header` when
    /// `X-Instance-Host` is absent, and `invalid_instance_header` when it is not a
    /// plausible host.
    pub fn from_headers(headers: &HeaderMap) -> Result<Self, ApiError> {
        let instance_host = headers
            .get(INSTANCE_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|host| is_plausible_host(host))
            .map(str::to_ascii_lowercase)
            .ok_or_else(|| {
                if headers.contains_key(INSTANCE_HEADER) {
                    ApiError::bad_request(
                        "invalid_instance_header",
                        "X-Instance-Host must be a bare host name such as mastodon.social",
                    )
                } else {
                    ApiError::bad_request(
                        "missing_instance_header",
                        "X-Instance-Host is required so the token can be verified",
                    )
                }
            })?;
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                value
                    .split_once(' ')
                    .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                    .map(|(_, token)| token.trim())
            })
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                ApiError::bad_request(
                    "missing_credentials",
                    "Authorization: Bearer <instance access token> is required",
                )
            })?;
        Ok(Self {
            instance_host,
            token: Token::new(token.to_owned()),
        })
    }
}
