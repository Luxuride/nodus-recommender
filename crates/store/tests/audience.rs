//! Integration tests for `account.audience_weight`, the reach count behind D24.
//!
//! The value is a whole-table aggregate over `account_affinity`, so the only
//! honest test is against Postgres: what matters is which rows the two `UPDATE`s
//! touch, and that an unchanged input costs nothing.

use nodus_store::affinity::{self, AffinityRow};
use nodus_store::audience::{self, AudienceRefresh};
use nodus_store::models::NewAccount;
use nodus_store::{accounts, users};
use sqlx::PgPool;
use uuid::Uuid;

/// One stored candidate, in the shape [`affinity::replace_for_user`] takes.
fn candidate(account_id: i64, affinity: f32) -> AffinityRow {
    AffinityRow {
        account_id,
        affinity,
        support: 1,
        sources: vec!["similar_user".to_owned()],
    }
}

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

/// Read the stored weight for one account.
async fn weight(pool: &PgPool, account_id: i64) -> i32 {
    sqlx::query_scalar("SELECT audience_weight FROM account WHERE id = $1")
        .bind(account_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn weight_equals_affinity_row_count(pool: PgPool) {
    let first = seed_account(&pool, "remote.test", 10, "alice").await;
    let second = seed_account(&pool, "remote.test", 11, "bob").await;
    let untouched = seed_account(&pool, "remote.test", 12, "carol").await;

    let (one, _) = seed_user(&pool, "home.test", 1, "one").await;
    let (two, _) = seed_user(&pool, "home.test", 2, "two").await;

    affinity::replace_for_user(&pool, one, &[candidate(first, 0.9), candidate(second, 0.4)])
        .await
        .unwrap();
    affinity::replace_for_user(&pool, two, &[candidate(first, 0.7)])
        .await
        .unwrap();

    let refresh = audience::recompute(&pool).await.unwrap();
    assert_eq!(
        refresh,
        AudienceRefresh {
            updated: 2,
            cleared: 0
        },
        "only the two accounts that gained affinity rows move"
    );

    assert_eq!(weight(&pool, first).await, 2);
    assert_eq!(weight(&pool, second).await, 1);
    assert_eq!(
        weight(&pool, untouched).await,
        0,
        "an account in nobody's set keeps the default"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn dropped_affinity_rows_fall_back_to_zero(pool: PgPool) {
    let shared = seed_account(&pool, "remote.test", 10, "alice").await;
    let only_mine = seed_account(&pool, "remote.test", 11, "bob").await;

    let (one, _) = seed_user(&pool, "home.test", 1, "one").await;
    let (two, _) = seed_user(&pool, "home.test", 2, "two").await;

    affinity::replace_for_user(&pool, one, &[candidate(shared, 0.9)])
        .await
        .unwrap();
    affinity::replace_for_user(
        &pool,
        two,
        &[candidate(shared, 0.5), candidate(only_mine, 0.3)],
    )
    .await
    .unwrap();

    let first = audience::recompute(&pool).await.unwrap();
    assert_eq!(
        first,
        AudienceRefresh {
            updated: 2,
            cleared: 0
        }
    );
    assert_eq!(weight(&pool, shared).await, 2);
    assert_eq!(weight(&pool, only_mine).await, 1);

    // A recompute that finds no neighbours replaces the user's set with nothing,
    // which is the path that leaves an account with no affinity rows at all.
    affinity::replace_for_user(&pool, two, &[]).await.unwrap();

    let second = audience::recompute(&pool).await.unwrap();
    assert_eq!(
        second,
        AudienceRefresh {
            updated: 1,
            cleared: 1
        },
        "the shared account is recounted and the orphaned one is cleared"
    );
    assert_eq!(weight(&pool, shared).await, 1);
    assert_eq!(weight(&pool, only_mine).await, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn unchanged_input_touches_no_row(pool: PgPool) {
    let first = seed_account(&pool, "remote.test", 10, "alice").await;
    let second = seed_account(&pool, "remote.test", 11, "bob").await;

    let (one, _) = seed_user(&pool, "home.test", 1, "one").await;
    let (two, _) = seed_user(&pool, "home.test", 2, "two").await;

    affinity::replace_for_user(&pool, one, &[candidate(first, 0.9)])
        .await
        .unwrap();
    affinity::replace_for_user(&pool, two, &[candidate(first, 0.6), candidate(second, 0.2)])
        .await
        .unwrap();

    let first_pass = audience::recompute(&pool).await.unwrap();
    assert_eq!(
        first_pass,
        AudienceRefresh {
            updated: 2,
            cleared: 0
        }
    );

    let second_pass = audience::recompute(&pool).await.unwrap();
    assert_eq!(
        second_pass,
        AudienceRefresh {
            updated: 0,
            cleared: 0
        },
        "the <> guards must make a repeated pass a no-op"
    );
    assert_eq!(weight(&pool, first).await, 2);
    assert_eq!(weight(&pool, second).await, 1);
}
