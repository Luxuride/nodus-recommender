//! Tests for the guards and the page split.

use axum::http::HeaderMap;
use nodus_shared::MIN_CONTRACT_VERSION;

use super::params::{contract_gate, degraded_flag, parse_version, split};
use crate::error::ApiError;

#[test]
fn versions_compare_numerically() {
    assert!(parse_version("0.2.0") > parse_version("0.1.9"));
    assert!(parse_version("1.0.0") > parse_version("0.99.99"));
    assert_eq!(parse_version("0"), (0, 0, 0));
    assert_eq!(parse_version("0.1.0-rc1"), (0, 1, 0));
}

#[test]
fn a_missing_header_passes_the_gate() {
    assert!(contract_gate(&HeaderMap::new()).is_ok());
}

#[test]
fn an_older_client_is_rejected() {
    let mut headers = HeaderMap::new();
    headers.insert("x-contract-version", "0.0.1".parse().unwrap());
    assert!(matches!(
        contract_gate(&headers),
        Err(ApiError::ClientTooOld { .. })
    ));
}

#[test]
fn a_current_client_passes() {
    let mut headers = HeaderMap::new();
    headers.insert("x-contract-version", MIN_CONTRACT_VERSION.parse().unwrap());
    assert!(contract_gate(&headers).is_ok());
}

#[test]
fn a_limit_plus_one_read_reports_more() {
    let (page, more) = split(vec![1, 2, 3], 2);
    assert_eq!(page, vec![1, 2]);
    assert!(more);
    let (page, more) = split(vec![1, 2], 2);
    assert_eq!(page, vec![1, 2]);
    assert!(!more);
}

#[test]
fn degradation_prefers_the_cause_over_the_symptom() {
    assert_eq!(
        degraded_flag(false, None).as_deref(),
        Some("no_server_sync")
    );
    assert_eq!(degraded_flag(true, None).as_deref(), Some("cold_start"));
    assert_eq!(
        degraded_flag(true, Some(72.0)).as_deref(),
        Some("stale_similarity")
    );
    assert_eq!(degraded_flag(true, Some(2.0)), None);
}
