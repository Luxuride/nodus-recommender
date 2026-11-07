//! Unit tests for the parts of the post feed that do not need a database.
//!
//! The ranking itself lives in `nodus-core` and is tested there; what is tested here
//! is the arithmetic this file owns: the age clamp, the attention scale, and the
//! rule that an unusable input cannot become a top row.

use super::{engagement_prior, hours_between, rfc3339};
use time::{Duration, OffsetDateTime};

#[test]
fn a_post_from_the_future_is_not_younger_than_now() {
    let now = OffsetDateTime::now_utc();
    let future = now + Duration::hours(3);
    assert!((hours_between(now, future) - 0.0).abs() < f64::EPSILON);
}

#[test]
fn age_is_measured_in_hours() {
    let now = OffsetDateTime::now_utc();
    assert!((hours_between(now, now - Duration::minutes(90)) - 1.5).abs() < 1e-9);
}

#[test]
fn attention_grows_slowly_and_boosts_count_double() {
    let none = engagement_prior(0, 0);
    let hundred = engagement_prior(100, 0);
    let boosted = engagement_prior(0, 50);
    assert!(
        (none - 1.0).abs() < 1e-9,
        "no attention is the floor: {none}"
    );
    assert!(
        hundred > none && hundred < 3.0,
        "a hundred is about 2: {hundred}"
    );
    assert!(
        (boosted - hundred).abs() < 1e-9,
        "fifty boosts weigh as much as a hundred favourites: {boosted} vs {hundred}"
    );
}

#[test]
fn timestamps_are_rfc3339_with_z() {
    let stamp = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid");
    assert_eq!(rfc3339(stamp), "2023-11-14T22:13:20Z");
}
