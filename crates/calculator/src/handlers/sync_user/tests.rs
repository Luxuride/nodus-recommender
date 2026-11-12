//! Token classification and budget bucket keys.
//!
//! These exercise the pure helpers behind the token loop; they never touch the
//! network or the database.

use nodus_client_core::Error as InstanceError;
use uuid::Uuid;

use super::accounts::Token;
use super::follows::{AttemptError, Retired, classify, transport};
use crate::handlers::JobError;

#[test]
fn a_401_retires_the_token_instead_of_failing_the_job() {
    assert!(matches!(
        classify(InstanceError::Unauthorized),
        AttemptError::Retired(Retired::Revoked)
    ));
}

#[test]
fn a_rate_limit_retires_the_token_with_the_instances_delay() {
    match classify(InstanceError::RateLimited { retry_after_s: 42 }) {
        AttemptError::Retired(Retired::RateLimited(delay)) => {
            assert_eq!(delay.as_secs(), 42);
        }
        other => panic!("expected a rate-limited token, got {other:?}"),
    }
}

#[test]
fn a_transport_failure_is_retried() {
    assert!(matches!(
        transport(InstanceError::Transport("timeout".to_owned())),
        JobError::Retry { .. }
    ));
}

#[test]
fn the_budget_bucket_is_the_lowercase_fingerprint_hex() {
    let token = Token::new(Uuid::nil(), "secret".to_owned());
    let bucket = token.bucket();
    assert_eq!(bucket.len(), 64);
    assert!(
        bucket
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
    );
}
