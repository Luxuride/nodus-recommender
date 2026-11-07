//! The numbers a recommendation engine is judged by, and the protocol that makes them
//! mean anything.
//!
//! Two rules do the work here.
//!
//! **Nothing is scored that the model could have seen.** The split is by
//! `follow_edge.first_seen_at` against one instant: an edge created before the cutoff
//! is training data, an edge created after it is the answer. Evaluating on a random
//! hold-out instead leaks the future into the past — a candidate set built from
//! today's graph already knows which accounts became popular, so recall looks fine
//! and the model learns nothing.
//!
//! **Accuracy is reported against a baseline and per follower bucket.** A follower
//! graph is dominated by a few hubs, so "precision@10 = 0.3" usually means "three
//! hubs"; the popularity baseline exists so that number can be read as an improvement
//! or as an artefact, and the buckets exist because recommending another hub is worth
//! less to a user than recommending an account nobody has heard of.

use std::collections::HashSet;

/// What one ranked list achieved for one user.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hit {
    /// Recommendations that were also followed after the cutoff.
    pub hits: u32,
    /// Recommendations shown, i.e. the effective `k`.
    pub shown: u32,
    /// Follows created after the cutoff that the model was trying to find.
    pub truth: u32,
}

impl Hit {
    /// Precision at the list's own length.
    #[must_use]
    pub fn precision(self) -> f64 {
        ratio(self.hits, self.shown)
    }

    /// Recall at the list's own length.
    #[must_use]
    pub fn recall(self) -> f64 {
        ratio(self.hits, self.truth)
    }
}

/// Aggregates one user's ranking against the truth set.
///
/// `ranked` is in score order, so a hit near the top counts the same as a hit at
/// position ten — precision@k is a set measure, and inventing a position discount
/// here would report a number no offline score actually optimised.
#[must_use]
pub fn score_user(ranked: &[i64], truth: &HashSet<i64>, k: usize) -> Hit {
    let shown: Vec<i64> = ranked.iter().copied().take(k).collect();
    let hit_set: HashSet<i64> = truth.clone();
    Hit {
        hits: u32::try_from(shown.iter().filter(|id| hit_set.contains(id)).count()).unwrap_or(0),
        shown: u32::try_from(shown.len()).unwrap_or(0),
        truth: u32::try_from(truth.len()).unwrap_or(0),
    }
}

/// A follower-count bucket, so a hub cannot stand in for the whole result.
///
/// The split at a thousand is not a discovery about Mastodon; it is the smallest
/// boundary that separates "a name everyone has heard of" from "an account someone
/// had to be told about" on a typical instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bucket {
    /// Fewer than a thousand followers: the accounts a feed is actually for.
    Small,
    /// A thousand to ten thousand.
    Medium,
    /// Ten thousand and up: hubs, where popularity and taste agree.
    Hub,
}

/// Bucket for a follower count.
#[must_use]
pub const fn bucket(followers: i64) -> Bucket {
    if followers < 1_000 {
        Bucket::Small
    } else if followers < 10_000 {
        Bucket::Medium
    } else {
        Bucket::Hub
    }
}

/// Hits and opportunities per bucket, so a report can say where the wins came from.
#[derive(Debug, Clone, Copy, Default)]
pub struct BucketTally {
    /// Recommended accounts in this bucket that were followed after the cutoff.
    pub hits: u32,
    /// Accounts followed after the cutoff that fall in this bucket.
    pub truth: u32,
}

impl BucketTally {
    /// Share of this bucket's truth set that was recommended.
    #[must_use]
    pub fn recall(self) -> f64 {
        ratio(self.hits, self.truth)
    }
}

/// Adds one user's result into the per-bucket tallies.
pub fn tally_buckets(
    ranked: &[i64],
    truth: &HashSet<i64>,
    followers: &std::collections::HashMap<i64, i64>,
    k: usize,
    tallies: &mut [BucketTally; 3],
) {
    for id in ranked.iter().copied().take(k) {
        if truth.contains(&id) {
            let index = index_of(bucket(followers.get(&id).copied().unwrap_or(0)));
            tallies[index].hits += 1;
        }
    }
    for id in truth {
        let index = index_of(bucket(followers.get(id).copied().unwrap_or(0)));
        tallies[index].truth += 1;
    }
}

/// Array index for a bucket, so the report can print in a fixed order.
#[must_use]
pub const fn index_of(bucket: Bucket) -> usize {
    match bucket {
        Bucket::Small => 0,
        Bucket::Medium => 1,
        Bucket::Hub => 2,
    }
}

/// Name used in the report.
#[must_use]
pub const fn bucket_name(index: usize) -> &'static str {
    match index {
        0 => "<1k followers",
        1 => "1k-10k",
        _ => ">=10k",
    }
}

/// Division that answers `0.0` instead of `NaN` when the denominator is zero.
///
/// A user with no truth set is not a failure of the ranking, and averaging a `NaN`
/// into the table would hide every other user's result.
fn ratio(numerator: u32, denominator: u32) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        f64::from(numerator) / f64::from(denominator)
    }
}

#[cfg(test)]
mod tests;
