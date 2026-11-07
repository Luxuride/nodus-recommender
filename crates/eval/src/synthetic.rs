//! A planted graph, so the harness can be checked before it is trusted.
//!
//! The point is not to simulate Mastodon. It is to build a graph where the right answer
//! is known: users are drawn from taste clusters, most follows stay inside the cluster,
//! and a fifth of the edges are stamped after the cutoff so they become the answer key.
//! A recommender that cannot beat the popularity baseline on data with an obvious taste
//! structure is broken, and a harness that reports otherwise is broken too.

use std::collections::HashMap;

use sqlx::PgPool;
use time::OffsetDateTime;

/// The host every synthetic account and user lives on, so cleanup is one predicate.
pub const HOST: &str = "eval.test";

/// How many accounts the synthetic graph creates per cluster.
#[derive(Debug, Clone, Copy)]
pub struct Shape {
    /// How many app users to create.
    pub users: usize,
    /// How many accounts to create (including the ones the users sign in as).
    pub accounts: usize,
    /// How many taste clusters the accounts are split across.
    pub clusters: usize,
    /// Fraction of edges stamped after the cutoff, in per mille.
    pub truth_per_mille: usize,
}

/// Deterministic linear congruential generator.
///
/// A real RNG would make a failing run impossible to reproduce; three constants and a
/// multiply are enough to make the planted graph varied.
#[derive(Debug)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        // 31 bits of the state, so the cast is exact and the value is < 1.
        f64::from(u32::try_from(self.next() & 0x7fff_ffff).unwrap_or(0)) / 2_147_483_648.0
    }

    /// Uniform integer in `[0, bound)`.
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        usize::try_from(self.next() % u64::try_from(bound).unwrap_or(1)).unwrap_or(0)
    }
}

/// Deletes every row this module has ever created.
///
/// Explicit `delete`s rather than a truncate: the harness is expected to run against a
/// database that also holds real development data.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn reset(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "DELETE FROM follow_edge WHERE follower_id IN (SELECT id FROM account WHERE instance_host = $1) \
            OR followee_id IN (SELECT id FROM account WHERE instance_host = $1)",
    )
    .bind(HOST)
    .execute(pool)
    .await?;
    sqlx::query("DELETE FROM app_user WHERE instance_host = $1")
        .bind(HOST)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM account WHERE instance_host = $1")
        .bind(HOST)
        .execute(pool)
        .await?;
    Ok(())
}

/// Creates the planted graph, replacing whatever was there.
///
/// # Errors
///
/// Propagates [`sqlx::Error`]; `clusters == 0` is rejected before any write.
pub async fn seed(pool: &PgPool, shape: Shape, seed: u64) -> Result<(), sqlx::Error> {
    if shape.clusters == 0 {
        return Err(sqlx::Error::Configuration(
            "clusters must be at least one".into(),
        ));
    }
    reset(pool).await?;
    sqlx::query(
        "INSERT INTO instance (host, software) VALUES ($1, 'mastodon') ON CONFLICT DO NOTHING",
    )
    .bind(HOST)
    .execute(pool)
    .await?;

    let mut rng = Rng(seed);
    let ids = insert_accounts(pool, shape, &mut rng).await?;

    let now = OffsetDateTime::now_utc();
    // Two instants, not a range: the split is by `first_seen_at`, so the timestamps
    // have to be unambiguous rather than randomly spread around the cutoff.
    let before = now - time::Duration::days(30);
    let after = now - time::Duration::hours(1);

    let mut edges: Vec<(i64, i64, OffsetDateTime)> = Vec::new();
    for user in 0..shape.users {
        let cluster = user % shape.clusters;
        let Some(&follower) = ids.get(&(user % shape.accounts)) else {
            continue;
        };
        for target in 0..shape.accounts {
            if target == user % shape.accounts {
                continue;
            }
            let target_cluster = target % shape.clusters;
            let chance = if target_cluster == cluster {
                0.45
            } else if target_cluster.abs_diff(cluster) == 1 {
                0.05
            } else {
                0.004
            };
            if rng.unit() >= chance {
                continue;
            }
            // One in five edges lands after the cutoff, so the truth set is non-trivial
            // but every user still has a history to be similar from.
            let stamp = if rng.below(1_000) < shape.truth_per_mille {
                after
            } else {
                before
            };
            let Some(&candidate) = ids.get(&target) else {
                continue;
            };
            edges.push((follower, candidate, stamp));
        }
    }

    for (follower, followee, stamp) in edges {
        sqlx::query(
            "INSERT INTO follow_edge (follower_id, followee_id, source, first_seen_at, last_seen_at) \
             VALUES ($1, $2, 'server_sync', $3, $3) ON CONFLICT DO NOTHING",
        )
        .bind(follower)
        .bind(followee)
        .bind(stamp)
        .execute(pool)
        .await?;
    }

    // The app users sign in as their own accounts, so `app_user` and `account` agree on
    // (instance_host, mastodon_account_id) — which is the join the graph loader uses.
    for user in 0..shape.users {
        let mastodon_id = i64::try_from(user + 1).unwrap_or(1);
        sqlx::query(
            "INSERT INTO app_user (instance_host, mastodon_account_id, acct) \
             VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(HOST)
        .bind(mastodon_id)
        .bind(format!("synthetic{user}"))
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Creates the accounts and returns `ordinal -> account id`.
///
/// Every tenth account is a hub, because the popularity baseline needs something
/// plausible to recommend: a baseline starved of candidates would make the comparison
/// flattering rather than honest.
async fn insert_accounts(
    pool: &PgPool,
    shape: Shape,
    rng: &mut Rng,
) -> Result<HashMap<usize, i64>, sqlx::Error> {
    for ordinal in 0..shape.accounts {
        let followers: i64 = if ordinal % 10 == 0 {
            20_000 + i64::try_from(rng.below(180_000)).unwrap_or(0)
        } else {
            40 + i64::try_from(rng.below(4_000)).unwrap_or(0)
        };
        sqlx::query(
            "INSERT INTO account (instance_host, mastodon_id, acct, display_name, followers_count) \
             VALUES ($1, $2, $3, $3, $4)",
        )
        .bind(HOST)
        .bind(i64::try_from(ordinal + 1).unwrap_or(0))
        .bind(format!("synthetic{ordinal}"))
        .bind(followers)
        .execute(pool)
        .await?;
    }

    Ok(sqlx::query_as::<_, (i64, i64)>(
        "SELECT mastodon_id, id FROM account WHERE instance_host = $1",
    )
    .bind(HOST)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|(mastodon_id, id)| {
        (
            usize::try_from(mastodon_id).unwrap_or(0).saturating_sub(1),
            id,
        )
    })
    .collect())
}
