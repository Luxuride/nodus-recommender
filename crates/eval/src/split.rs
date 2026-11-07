//! The evaluation split: one instant, and everything on either side of it.
//!
//! The cutoff is the only argument that matters. `follow_edge.first_seen_at` is the
//! timestamp the crawler observed the edge, so edges before the cutoff are the graph
//! the model could have seen and edges after it are the answers. That is the
//! difference between measuring a recommender and measuring a lookup table.

use std::collections::{HashMap, HashSet};

use sqlx::PgPool;
use time::OffsetDateTime;

/// An app user that has an account row, i.e. one this evaluation can score.
#[derive(Debug, Clone, Copy)]
pub struct UserRow {
    /// The Mastodon account the user signs in as.
    pub account_id: i64,
}

/// The whole graph at a cutoff, in memory.
#[derive(Debug, Default)]
pub struct Graph {
    /// Follower counts, which are the IDF weights and the popularity baseline.
    pub followers: HashMap<i64, i64>,
    /// Users with an account, in a stable order.
    pub users: Vec<UserRow>,
    /// Training edges: follower account → followees, all seen before the cutoff.
    pub training: HashMap<i64, Vec<i64>>,
    /// Truth edges: follower account → followees first seen at or after the cutoff.
    pub truth: HashMap<i64, Vec<i64>>,
}

/// Reads the graph either side of `cutoff`.
///
/// # Errors
///
/// Propagates [`sqlx::Error`]; a missing table means the migrations have not run.
pub async fn load(pool: &PgPool, cutoff: OffsetDateTime) -> Result<Graph, sqlx::Error> {
    // `followers_count` is an `int4` in the schema; reading it as `i64` would fail at
    // decode time rather than at review time, so the widening happens here.
    let followers: Vec<(i64, i32)> = sqlx::query_as("SELECT id, followers_count FROM account")
        .fetch_all(pool)
        .await?;
    let users: Vec<(i64,)> = sqlx::query_as(
        "SELECT a.id \
         FROM app_user u \
         JOIN account a \
           ON a.instance_host = u.instance_host \
          AND a.mastodon_id = u.mastodon_account_id \
         WHERE u.deleted_at IS NULL \
         ORDER BY u.created_at, u.id",
    )
    .fetch_all(pool)
    .await?;
    let training: Vec<(i64, i64)> =
        sqlx::query_as("SELECT follower_id, followee_id FROM follow_edge WHERE first_seen_at < $1")
            .bind(cutoff)
            .fetch_all(pool)
            .await?;
    let truth: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT follower_id, followee_id FROM follow_edge WHERE first_seen_at >= $1",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;

    Ok(Graph {
        followers: followers
            .into_iter()
            .map(|(id, count)| (id, i64::from(count)))
            .collect(),
        users: users
            .into_iter()
            .map(|(account_id,)| UserRow { account_id })
            .collect(),
        training: group(training),
        truth: group(truth),
    })
}

/// Groups edges by follower, sorted so a report prints the same graph in the same order.
fn group(edges: Vec<(i64, i64)>) -> HashMap<i64, Vec<i64>> {
    let mut grouped: HashMap<i64, Vec<i64>> = HashMap::new();
    for (follower, followee) in edges {
        grouped.entry(follower).or_default().push(followee);
    }
    for followees in grouped.values_mut() {
        followees.sort_unstable();
        followees.dedup();
    }
    grouped
}

impl Graph {
    /// Accounts this follower had already followed by the cutoff.
    #[must_use]
    pub fn training_follows(&self, account_id: i64) -> &[i64] {
        self.training
            .get(&account_id)
            .map_or(&[], std::vec::Vec::as_slice)
    }

    /// Accounts this follower started following after the cutoff — the answer key.
    #[must_use]
    pub fn truth_set(&self, account_id: i64) -> HashSet<i64> {
        self.truth
            .get(&account_id)
            .map(|followees| followees.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Follower count, treating an unknown account as having none.
    #[must_use]
    pub fn followers_of(&self, account_id: i64) -> u32 {
        u32::try_from(self.followers.get(&account_id).copied().unwrap_or(0)).unwrap_or(0)
    }

    /// Users that have at least `minimum` training follows and something to find.
    ///
    /// A user with three follows has no similar-user signal to propagate and a user
    /// whose truth set is empty can never be a hit; both would drag the averages
    /// toward zero for reasons that say nothing about the ranking.
    #[must_use]
    pub fn scoreable(&self, minimum: usize) -> Vec<UserRow> {
        self.users
            .iter()
            .copied()
            .filter(|user| self.training_follows(user.account_id).len() >= minimum)
            .filter(|user| !self.truth_set(user.account_id).is_empty())
            .collect()
    }
}
