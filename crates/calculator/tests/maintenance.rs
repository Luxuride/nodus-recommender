//! Integration tests for the maintenance tick.
//!
//! The tick's value is in the SQL it produces, so these run against a real Postgres:
//! a mocked queue would only prove the mock was called.

use nodus_calculator::maintenance::{Maintenance, USERS_PER_TICK};
use nodus_store::models::NewAccount;
use nodus_store::{accounts, jobs, users};
use sqlx::PgPool;

async fn seed_user(pool: &PgPool, host: &str, mastodon_id: i64, acct: &str) -> (uuid::Uuid, i64) {
    users::ensure_instance(pool, host).await.expect("instance");
    let user = users::upsert(pool, host, mastodon_id, acct)
        .await
        .expect("user");
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
    .expect("own account");
    let account = accounts::find_id(pool, host, mastodon_id)
        .await
        .expect("find own account")
        .expect("own account exists");
    (user.id, account)
}

async fn seed_account(pool: &PgPool, host: &str, mastodon_id: i64, acct: &str) -> i64 {
    users::ensure_instance(pool, host).await.expect("instance");
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
    .expect("account");
    accounts::find_id(pool, host, mastodon_id)
        .await
        .expect("find account")
        .expect("account exists")
}

/// Count the queued rows for one kind.
async fn queued(pool: &PgPool, kind: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM job WHERE kind = $1 AND status = 'pending'")
        .bind(kind)
        .fetch_one(pool)
        .await
        .expect("count jobs")
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_tick_queues_the_instance_passes(pool: PgPool) {
    let maintenance = Maintenance::new(pool.clone());

    let report = maintenance.tick().await.expect("tick");

    assert!(report.housekeeping, "the instance passes run on every tick");
    assert_eq!(report.users, 0, "nobody changed their graph yet");
    assert_eq!(queued(&pool, "recompute_follow_through").await, 1);
    assert_eq!(queued(&pool, "retain").await, 1);
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_tick_reschedules_users_whose_graph_moved(pool: PgPool) {
    let (user_id, me) = seed_user(&pool, "local.test", 1, "me").await;
    let friend = seed_account(&pool, "remote.test", 100, "friend").await;
    let other = seed_account(&pool, "remote.test", 200, "other").await;
    accounts::replace_follows(&pool, me, &[friend, other], "server_sync")
        .await
        .expect("edges");

    let report = Maintenance::new(pool.clone()).tick().await.expect("tick");

    assert_eq!(report.users, 1, "one user changed their graph");
    let subject = sqlx::query_scalar::<_, Option<String>>(
        "SELECT subject_id FROM job WHERE kind = 'recompute_similarity' AND status = 'pending'",
    )
    .fetch_one(&pool)
    .await
    .expect("job row");
    assert_eq!(subject.as_deref(), Some(user_id.to_string().as_str()));
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_tick_queues_the_statuses_of_followed_authors(pool: PgPool) {
    let (_user_id, me) = seed_user(&pool, "local.test", 1, "me").await;
    let friend = seed_account(&pool, "remote.test", 100, "friend").await;
    accounts::replace_follows(&pool, me, &[friend], "server_sync")
        .await
        .expect("edges");

    let report = Maintenance::new(pool.clone()).tick().await.expect("tick");

    assert_eq!(report.statuses, 1, "the followed author is due");
    let subject: Option<String> = sqlx::query_scalar(
        "SELECT subject_id FROM job WHERE kind = 'refresh_account' AND status = 'pending'",
    )
    .fetch_one(&pool)
    .await
    .expect("job row");
    assert_eq!(subject.as_deref(), Some(friend.to_string().as_str()));
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_second_tick_does_not_duplicate_the_queue(pool: PgPool) {
    let (_user_id, me) = seed_user(&pool, "local.test", 1, "me").await;
    let friend = seed_account(&pool, "remote.test", 100, "friend").await;
    accounts::replace_follows(&pool, me, &[friend], "server_sync")
        .await
        .expect("edges");

    let maintenance = Maintenance::new(pool.clone());
    maintenance.tick().await.expect("first tick");
    let report = maintenance.tick().await.expect("second tick");

    assert_eq!(
        report.users, 0,
        "the queue's partial unique index already holds the job"
    );
    assert_eq!(
        queued(&pool, "recompute_similarity").await,
        1,
        "one row per user, not one per tick"
    );
    assert_eq!(queued(&pool, "recompute_follow_through").await, 1);
}

#[sqlx::test(migrations = "../store/migrations")]
async fn the_tick_is_bounded(pool: PgPool) {
    // Every user's graph moved, but one tick may only reschedule a bounded number:
    // a producer that outruns its workers turns a backlog into a landslide.
    let users_to_seed = usize::try_from(USERS_PER_TICK).expect("bounded") + 3;
    for index in 0..users_to_seed {
        let mastodon_id = i64::try_from(index).expect("small index") + 1;
        let acct = format!("user{index}");
        let (_id, me) = seed_user(&pool, "local.test", mastodon_id, &acct).await;
        let followee = seed_account(
            &pool,
            "remote.test",
            10_000 + mastodon_id,
            &format!("f{index}"),
        )
        .await;
        accounts::replace_follows(&pool, me, &[followee], "server_sync")
            .await
            .expect("edge");
    }

    let report = Maintenance::new(pool.clone()).tick().await.expect("tick");

    assert_eq!(
        report.users,
        usize::try_from(USERS_PER_TICK).expect("bounded"),
        "one tick reschedules at most USERS_PER_TICK users"
    );
    let _ = jobs::backlog_by_priority(&pool).await.expect("backlog");
}
