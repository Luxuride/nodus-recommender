//! Tests for the metric arithmetic.

use std::collections::HashSet;

use super::{Bucket, BucketTally, Hit, bucket, index_of, score_user};

fn truth(ids: &[i64]) -> HashSet<i64> {
    ids.iter().copied().collect()
}

#[test]
fn precision_counts_the_list_not_the_scores() {
    let ranked = vec![1, 2, 3, 4];
    let hit = score_user(&ranked, &truth(&[2, 4, 9]), 3);
    assert_eq!(hit.hits, 1);
    assert_eq!(hit.shown, 3);
    assert_eq!(hit.truth, 3);
    assert!((hit.precision() - 1.0 / 3.0).abs() < 1e-12);
    assert!((hit.recall() - 1.0 / 3.0).abs() < 1e-12);
}

#[test]
fn an_empty_truth_set_scores_zero_rather_than_nan() {
    let hit = score_user(&[1, 2], &HashSet::new(), 10);
    assert_eq!(
        hit,
        Hit {
            hits: 0,
            shown: 2,
            truth: 0
        }
    );
    assert!(!hit.recall().is_nan());
}

#[test]
fn a_short_list_is_not_punished_for_being_short() {
    let hit = score_user(&[7], &truth(&[7]), 10);
    assert_eq!(hit.shown, 1);
    assert!((hit.precision() - 1.0).abs() < 1e-12);
}

#[test]
fn buckets_split_at_a_thousand_and_ten_thousand() {
    assert_eq!(bucket(999), Bucket::Small);
    assert_eq!(bucket(1_000), Bucket::Medium);
    assert_eq!(bucket(9_999), Bucket::Medium);
    assert_eq!(bucket(10_000), Bucket::Hub);
    assert_eq!(index_of(Bucket::Small), 0);
    assert_eq!(index_of(Bucket::Hub), 2);
}

#[test]
fn bucket_tallies_count_hits_and_opportunities_separately() {
    let mut followers = std::collections::HashMap::new();
    followers.insert(1, 10);
    followers.insert(2, 5_000);
    followers.insert(3, 50_000);
    let mut tallies = [BucketTally::default(); 3];
    super::tally_buckets(&[1, 2, 3], &truth(&[1, 2, 3]), &followers, 3, &mut tallies);
    assert_eq!(tallies[0].hits, 1);
    assert_eq!(tallies[0].truth, 1);
    assert_eq!(tallies[2].hits, 1);
    assert!((tallies[1].recall() - 1.0).abs() < 1e-12);
}

#[test]
fn a_missing_follower_count_is_treated_as_small() {
    let mut tallies = [BucketTally::default(); 3];
    super::tally_buckets(
        &[9],
        &truth(&[9]),
        &std::collections::HashMap::new(),
        1,
        &mut tallies,
    );
    assert_eq!(tallies[0].hits, 1);
}
