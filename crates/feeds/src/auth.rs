//! Who is calling, and how do we know.
//!
//! Nodus holds no accounts of its own. The caller proves who they are by
//! presenting the access token they already have for their instance, and we learn
//! their identity by asking that instance. A token is opaque, so the request also
//! has to say *which* instance to ask — hence the `X-Instance-Host` header.
//!
//! Verification is cached for ten minutes keyed by the token's fingerprint. That is
//! the compromise between authenticity (an instance call per request would triple
//! our latency and spend the user's rate limit) and revocation (a revoked token is
//! honoured for at most ten minutes, and any 401 from the instance clears the entry
//! immediately).

use axum::http::{HeaderMap, header};
use serde::Deserialize;

use crate::error::ApiError;
use crate::state::{AppState, Identity};
use nodus_store::crypto::fingerprint;

/// Header naming the instance the token belongs to.
pub const INSTANCE_HEADER: &str = "x-instance-host";

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

/// A verified caller: their representation in our database.
#[derive(Debug)]
pub struct Authenticated {
    /// Row in `app_user`, created on first sight.
    pub user: nodus_store::models::AppUser,
    /// The credentials the request presented, for handlers that echo settings back.
    pub credentials: Credentials,
}

/// Verify the presented token and resolve it to a user row.
///
/// # Errors
///
/// [`ApiError::Unauthorized`] when the instance rejects the token,
/// [`ApiError::Unavailable`] when the instance cannot be reached or is rate limiting
/// us, and [`ApiError::Internal`] for database failures.
pub async fn authenticate(
    state: &AppState,
    credentials: Credentials,
) -> Result<Authenticated, ApiError> {
    let key = fingerprint(credentials.token.expose());
    let identity = if let Some(cached) = state.identities.get(&key).await {
        cached
    } else {
        let verified = verify_at_instance(state, &credentials).await?;
        state.identities.insert(key, verified.clone()).await;
        verified
    };
    nodus_store::users::ensure_instance(&state.pool, &credentials.instance_host).await?;
    let user = nodus_store::users::upsert(
        &state.pool,
        &credentials.instance_host,
        identity.mastodon_account_id,
        &identity.acct,
    )
    .await?;
    Ok(Authenticated { user, credentials })
}

#[derive(Debug, Deserialize)]
struct VerifyResponse {
    /// Mastodon sends account ids as strings; treating them as numbers is exactly the
    /// kind of thing to get wrong when a field happens to be numeric today.
    id: String,
    acct: String,
}

async fn verify_at_instance(
    state: &AppState,
    credentials: &Credentials,
) -> Result<Identity, ApiError> {
    let url = format!(
        "https://{}/api/v1/accounts/verify_credentials",
        credentials.instance_host
    );
    let response = state
        .http
        .get(&url)
        .bearer_auth(credentials.token.expose())
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| {
            tracing::warn!(
                host = %credentials.instance_host,
                error = %error,
                "instance unreachable while verifying a token"
            );
            ApiError::unavailable("instance_unreachable", 30)
        })?;

    match response.status().as_u16() {
        200 => {
            let body: VerifyResponse = response.json().await.map_err(|error| {
                tracing::warn!(
                    host = %credentials.instance_host,
                    error = %error,
                    "instance returned an unreadable verify_credentials body"
                );
                ApiError::unavailable("instance_unreachable", 30)
            })?;
            let mashdoton_account_id = body.id.parse::<i64>().map_err(|_| {
                tracing::warn!(
                    host = %credentials.instance_host,
                    id = %body.id,
                    "instance returned a non-numeric account id"
                );
                ApiError::unavailable("instance_unreachable", 30)
            })?;
            Ok(Identity {
                mastodon_account_id: mashdoton_account_id,
                acct: body.acct,
            })
        }
        401 | 403 | 404 => Err(ApiError::Unauthorized),
        429 => {
            let retry_after_s = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(30);
            Err(ApiError::unavailable(
                "instance_rate_limited",
                retry_after_s,
            ))
        }
        status => {
            tracing::warn!(
                host = %credentials.instance_host,
                status,
                "instance failed while verifying a token"
            );
            Err(ApiError::unavailable("instance_unreachable", 30))
        }
    }
}

/// Reject anything that is not a bare host name.
///
/// The host is interpolated into a URL, so allowing a scheme, a path, a userinfo
/// section or whitespace would let a caller point our verified request at a host of
/// their choosing.
fn is_plausible_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('.')
        && !host.contains('/')
        && !host.contains('@')
        && !host.contains(' ')
        && !host.contains("://")
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']'))
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::{Credentials, is_plausible_host};

    fn headers(entries: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in entries {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("valid name"),
                HeaderValue::from_str(value).expect("valid value"),
            );
        }
        map
    }

    #[test]
    fn a_bearer_token_and_a_host_are_accepted() {
        let credentials = Credentials::from_headers(&headers(&[
            ("authorization", "Bearer tok-123"),
            ("x-instance-host", "Mastodon.Social"),
        ]))
        .expect("credentials parse");
        assert_eq!(credentials.instance_host, "mastodon.social");
        assert_eq!(credentials.token.expose(), "tok-123");
        assert!(format!("{:?}", credentials.token).contains("redacted"));
    }

    #[test]
    fn a_missing_authorization_header_is_named() {
        let error = Credentials::from_headers(&headers(&[("x-instance-host", "a.example")]))
            .expect_err("no credentials");
        assert_eq!(error.code(), "missing_credentials");
    }

    #[test]
    fn a_non_bearer_scheme_is_rejected() {
        let error = Credentials::from_headers(&headers(&[
            ("authorization", "Basic dXNlcjpwYXNz"),
            ("x-instance-host", "a.example"),
        ]))
        .expect_err("wrong scheme");
        assert_eq!(error.code(), "missing_credentials");
    }

    #[test]
    fn a_missing_instance_header_is_named() {
        let error = Credentials::from_headers(&headers(&[("authorization", "Bearer t")]))
            .expect_err("no host");
        assert_eq!(error.code(), "missing_instance_header");
    }

    #[test]
    fn a_host_that_could_escape_the_url_is_rejected() {
        for host in [
            "https://evil.example",
            "evil.example/path",
            "user@evil.example",
            "evil example",
        ] {
            let error = Credentials::from_headers(&headers(&[
                ("authorization", "Bearer t"),
                ("x-instance-host", host),
            ]))
            .expect_err("host must be rejected");
            assert_eq!(error.code(), "invalid_instance_header", "{host}");
        }
    }

    #[test]
    fn plausible_hosts_are_accepted() {
        for host in ["mastodon.social", "localhost:3000", "social.example.org"] {
            assert!(is_plausible_host(host), "{host}");
        }
    }

    #[test]
    fn the_authorization_header_name_is_used_from_the_http_crate() {
        assert_eq!(header::AUTHORIZATION.as_str(), "authorization");
    }
}
