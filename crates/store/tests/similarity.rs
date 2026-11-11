//! Integration tests for the similarity store functions.
//!
//! These run against a real Postgres because the value is the SQL: the neighbour
//! query is a self-join with a grouped count, and replacement is a transaction. A
//! mocked driver would only prove that the mock was called.

use nodus_core::similarity::{SimilarityWeights, blend, compare};
use nodus_store::models::NewAccount;
use nodus_store::similarity::{self, NeighbourRow};
use nodus_store::{accounts, users};
use sqlx::PgPool;
use uuid::Uuid;

/// Seed an `app_user` and its matching `account`, returning both ids.
async fn seed_user(pool: &PgPool, host: &str, mastodon_id: i64, acct: &str) -> (Uuid, i64) {
    users::ensure_instance(pool, host).await.unwrap();
    let user = users::upsert(pool, host, mastodon_id, acct).await.unwrap();
    accounts::upsert_many(
        pool,
        &[NewAccount {
            instance_host: host.to_owned(),
            mastodon_id,
            acct: acct.to_owned(),
            display_name: None,
            followers_count: 0,
            bot: false,
            locked: false,
            discoverable: None,
            indexable: None,
            noindex: None,
        }],
    )
    .await
    .unwrap();
    let account = accounts::find_id(pool, host, mastodon_id)
        .await
        .unwrap()
        .unwrap();
    (user.id, account)
}

/// Seed one remote account and return its local id.
async fn seed_account(pool: &PgPool, host: &str, mastodon_id: i64, acct: &str) -> i64 {
    users::ensure_instance(pool, host).await.unwrap();
    accounts::upsert_many(
        pool,
        &[NewAccount {
            instance_host: host.to_owned(),
            mastodon_id,
            acct: acct.to_owned(),
            display_name: None,
            followers_count: 0,
            bot: false,
            locked: false,
            discoverable: None,
            indexable: None,
            noindex: None,
        }],
    )
    .await
    .unwrap();
    accounts::find_id(pool, host, mastodon_id)
        .await
        .unwrap()
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn two_shared_follows_score_higher_than_one(pool: PgPool) {
    let first = seed_account(&pool, "remote.test", 10, "alice").await;
    let second = seed_account(&pool, "remote.test", 11, "bob").await;
    let third = seed_account(&pool, "remote.test", 12, "carol").await;

    let (me, me_account) = seed_user(&pool, "home.test", 1, "me").await;
    let (close, close_account) = seed_user(&pool, "home.test", 2, "close").await;
    let (far, far_account) = seed_user(&pool, "home.test", 3, "far").await;

    // `me` and `close` share two follows; `me` and `far` share only one.
    accounts::replace_follows(&pool, me_account, &[first, second], "server_sync")
        .await
        .unwrap();
    accounts::replace_follows(&pool, close_account, &[first, second], "server_sync")
        .await
        .unwrap();
    accounts::replace_follows(&pool, far_account, &[first, third], "server_sync")
        .await
        .unwrap();

    let mine = similarity::graph(&pool, me)
        .await
        .unwrap()
        .expect("the user has an account row");
    let close_graph = similarity::graph(&pool, close)
        .await
        .unwrap()
        .expect("the user has an account row");
    let far_graph = similarity::graph(&pool, far)
        .await
        .unwrap()
        .expect("the user has an account row");

    let weights = SimilarityWeights::default();
    let close_score = blend(
        &compare(&mine.signals, &close_graph.signals, weights),
        weights,
    );
    let far_score = blend(
        &compare(&mine.signals, &far_graph.signals, weights),
        weights,
    );

    assert!(
        close_score > far_score,
        "two shared follows ({close_score}) must beat one ({far_score})"
    );

    // The neighbour query agrees about who overlaps more.
    let neighbours = similarity::co_followers(&pool, me, 10).await.unwrap();
    assert_eq!(neighbours.first().copied(), Some(close));
}

#[sqlx::test(migrations = "./migrations")]
async fn replace_neighbours_is_idempotent(pool: PgPool) {
    let (me, _) = seed_user(&pool, "home.test", 1, "me").await;
    let (other, _) = seed_user(&pool, "home.test", 2, "other").await;

    let rows = [NeighbourRow {
        user_id: me,
        similar_user_id: other,
        similarity: 0.5,
        co_follow: 0.6,
        co_fav: 0.1,
        co_reply: 0.0,
        reciprocity: 0.6,
    }];

    let first = similarity::replace_neighbours(&pool, me, &rows)
        .await
        .unwrap();
    assert_eq!(first, 1);
    let second = similarity::replace_neighbours(&pool, me, &rows)
        .await
        .unwrap();
    assert_eq!(second, 1);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM user_similarity WHERE user_id = $1")
        .bind(me)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "a repeated replace must not duplicate rows");
}
