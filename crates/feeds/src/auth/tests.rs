//! Tests for credential parsing and host validation.

use axum::http::{HeaderMap, HeaderValue, header};

use super::credentials::Credentials;
use super::headers::is_plausible_host;

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
    let error =
        Credentials::from_headers(&headers(&[("authorization", "Bearer t")])).expect_err("no host");
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
