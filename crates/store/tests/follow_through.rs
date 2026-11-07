//! Integration tests for the follow-through counters.
//!
//! The counters are the evidence half of D29: they only mean anything if the SQL
//! counts impressions and follows separately, keeps them per account, and forgets
//! evidence that fell out of the window. A mocked driver would test the mock.

use nodus_store::follow_through::{self, FollowThrough};
use nodus_store::models::NewAccount;
use nodus_store::{accounts, users};
use sqlx::PgPool;
use uuid::Uuid;

fn new_account(host: &str, mastodon_id: i64, acct: &str) -> NewAccount {
    NewAccount {
        instance_host: host.to_owned(),
        mastodon_id,
        acct: acct.to_owned(),
        display_name: None,
        followers_count: 0,
        bot: false,
        locked: false,
    }
}

/// Mints one account recommendation and returns its item id.
async fn item(pool: &PgPool, user: Uuid, account_id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO recommendation_item \
             (user_id, account_id, kind, score, reason, model_version) \
         VALUES ($1, $2, 'account', 0.5, '{}'::jsonb, 0) RETURNING id",
    )
    .bind(user)
    .bind(account_id)
    .fetch_one(pool)
    .await
    .expect("insert a recommendation item")
}

/// Records one event, aged by `age_days` so the window can be tested directly.
async fn event(pool: &PgPool, user: Uuid, item_id: i64, kind: &str, age_days: i32) {
    sqlx::query(
        "INSERT INTO interaction_event \
             (user_id, item_id, kind, client_ts, server_ts, idempotency_key) \
         VALUES ($1, $2, $3, now() - ($4::int * interval '1 day'), \
                 now() - ($4::int * interval '1 day'), uuid_send(gen_random_uuid()))",
    )
    .bind(user)
    .bind(item_id)
    .bind(kind)
    .bind(age_days)
    .execute(pool)
    .await
    .expect("record an event");
}

#[sqlx::test(migrations = "./migrations")]
async fn counters_count_impressions_and_follows_per_account(pool: PgPool) {
    users::ensure_instance(&pool, "home.test")
        .await
        .expect("ensure the home instance");
    users::ensure_instance(&pool, "remote.test")
        .await
        .expect("ensure the remote instance");
    let me = users::upsert(&pool, "home.test", 1, "me")
        .await
        .expect("create the first user");
    let other = users::upsert(&pool, "home.test", 2, "other")
        .await
        .expect("create the second user");

    accounts::upsert_many(
        &pool,
        &[
            new_account("home.test", 1, "me"),
            new_account("home.test", 2, "other"),
            new_account("remote.test", 10, "alice"),
            new_account("remote.test", 11, "bob"),
        ],
    )
    .await
    .expect("seed the accounts");

    let alice = accounts::find_id(&pool, "remote.test", 10)
        .await
        .expect("read the account id")
        .expect("alice exists");
    let bob = accounts::find_id(&pool, "remote.test", 11)
        .await
        .expect("read the account id")
        .expect("bob exists");

    let alice_for_me = item(&pool, me.id, alice).await;
    let alice_for_other = item(&pool, other.id, alice).await;
    let bob_for_me = item(&pool, me.id, bob).await;

    event(&pool, me.id, alice_for_me, "impression", 1).await;
    event(&pool, other.id, alice_for_other, "impression", 1).await;
    event(&pool, me.id, alice_for_me, "follow", 1).await;
    event(&pool, me.id, bob_for_me, "impression", 2).await;
    event(&pool, me.id, bob_for_me, "impression", 2).await;

    let report = follow_through::recompute(&pool).await.expect("recompute");
    assert_eq!(report.accounts, 2, "two accounts were recommended");
    assert_eq!(report.pruned, 0, "nothing to forget yet");
    let expected = report.expected_rate.expect("one follow out of five events");
    assert!(
        (expected - 0.2).abs() < 1e-9,
        "the baseline is follows over impressions, got {expected}"
    );

    let alice_row = follow_through::load(&pool, alice)
        .await
        .expect("read the counters")
        .expect("alice has counters");
    assert_eq!(alice_row.impressions, 2);
    assert_eq!(alice_row.follows, 1);
    assert_eq!(
        alice_row.distinct_users, 2,
        "two users produced impressions"
    );
    assert!(
        (alice_row.observed_rate().expect("alice was shown") - 0.5).abs() < 1e-9,
        "one follow in two impressions"
    );
    assert!(
        (alice_row.expected_rate.expect("a baseline was stored") - 0.2).abs() < 1e-9,
        "the baseline is stored with the row"
    );

    let bob_row = follow_through::load(&pool, bob)
        .await
        .expect("read the counters")
        .expect("bob has counters");
    assert_eq!(bob_row.impressions, 2);
    assert_eq!(bob_row.follows, 0, "shown twice, never followed");
    assert_eq!(bob_row.distinct_users, 1);
    assert!((bob_row.observed_rate().expect("bob was shown") - 0.0).abs() < 1e-9);
}

#[sqlx::test(migrations = "./migrations")]
async fn events_outside_the_window_are_ignored_and_stale_rows_are_pruned(pool: PgPool) {
    users::ensure_instance(&pool, "home.test")
        .await
        .expect("ensure the home instance");
    users::ensure_instance(&pool, "remote.test")
        .await
        .expect("ensure the remote instance");
    let me = users::upsert(&pool, "home.test", 1, "me")
        .await
        .expect("create the first user");

    accounts::upsert_many(
        &pool,
        &[
            new_account("home.test", 1, "me"),
            new_account("remote.test", 10, "carol"),
            new_account("remote.test", 11, "dave"),
        ],
    )
    .await
    .expect("seed the accounts");

    let carol = accounts::find_id(&pool, "remote.test", 10)
        .await
        .expect("read the account id")
        .expect("carol exists");
    let dave = accounts::find_id(&pool, "remote.test", 11)
        .await
        .expect("read the account id")
        .expect("dave exists");

    let carol_item = item(&pool, me.id, carol).await;
    event(&pool, me.id, carol_item, "impression", 120).await;
    event(&pool, me.id, carol_item, "follow", 120).await;

    // A row from an earlier run whose evidence has since aged out.
    sqlx::query(
        "INSERT INTO account_follow_through (account_id, impressions, follows, distinct_users) \
         VALUES ($1, 7, 1, 3)",
    )
    .bind(dave)
    .execute(&pool)
    .await
    .expect("seed a stale row");

    let report = follow_through::recompute(&pool).await.expect("recompute");
    assert_eq!(report.accounts, 0, "the only events are outside the window");
    assert_eq!(report.pruned, 1, "the stale row is forgotten");
    assert_eq!(report.expected_rate, None, "no evidence, no baseline");
    assert!(
        follow_through::load(&pool, carol)
            .await
            .expect("read the counters")
            .is_none()
    );
    assert!(
        follow_through::load(&pool, dave)
            .await
            .expect("read the counters")
            .is_none()
    );
}

#[test]
fn an_account_that_was_never_shown_has_no_rate() {
    let unseen = FollowThrough {
        impressions: 0,
        follows: 0,
        distinct_users: 0,
        expected_rate: Some(0.1),
    };
    assert_eq!(
        unseen.observed_rate(),
        None,
        "an unshown account is not evidence of anything, least of all failure"
    );

    let seen_once = FollowThrough {
        impressions: 4,
        follows: 1,
        distinct_users: 3,
        expected_rate: None,
    };
    assert!((seen_once.observed_rate().expect("it was shown once") - 0.25).abs() < 1e-9);
}
