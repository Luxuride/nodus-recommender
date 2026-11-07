//! Integration tests for the feed read and write paths.
//!
//! These run against a real Postgres, because the value of this module is the SQL: the
//! exclusions have to be *one* query (an `EXISTS` subquery per row would be a scan per
//! candidate) and the snapshot has to be readable by item id. A mocked driver would
//! test the mock.

use nodus_store::accounts;
use nodus_store::feed::{self, NewItem};
use nodus_store::models::NewAccount;
use nodus_store::users;
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

#[sqlx::test(migrations = "./migrations")]
async fn a_rejection_is_reported_rather_than_hidden(pool: PgPool) {
    users::ensure_instance(&pool, "home.test").await.unwrap();
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    let user = users::upsert(&pool, "home.test", 1, "me").await.unwrap();

    accounts::upsert_many(
        &pool,
        &[
            new_account("home.test", 1, "me"),
            new_account("remote.test", 10, "alice"),
            new_account("remote.test", 11, "bob"),
            new_account("remote.test", 12, "carol"),
            new_account("remote.test", 13, "dave"),
            new_account("remote.test", 14, "erin"),
        ],
    )
    .await
    .unwrap();

    let me = accounts::find_id(&pool, "home.test", 1)
        .await
        .unwrap()
        .unwrap();
    let alice = accounts::find_id(&pool, "remote.test", 10)
        .await
        .unwrap()
        .unwrap();
    let bob = accounts::find_id(&pool, "remote.test", 11)
        .await
        .unwrap()
        .unwrap();
    let carol = accounts::find_id(&pool, "remote.test", 12)
        .await
        .unwrap()
        .unwrap();
    let dave = accounts::find_id(&pool, "remote.test", 13)
        .await
        .unwrap()
        .unwrap();
    let erin = accounts::find_id(&pool, "remote.test", 14)
        .await
        .unwrap()
        .unwrap();

    // carol is already followed and erin's affinity expired; dave was dismissed but
    // must still come back as a candidate, carrying the rejection so the ranker can
    // decay it instead of the query hiding him forever (D26).
    accounts::replace_follows(&pool, me, &[carol], "server_sync")
        .await
        .unwrap();

    let dismissed_item = sqlx::query_scalar::<_, i64>(
        "INSERT INTO recommendation_item (user_id, account_id, kind, score, model_version) \
         VALUES ($1, $2, 'account', 0.4, 0) RETURNING id",
    )
    .bind(user.id)
    .bind(dave)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO interaction_event (user_id, item_id, kind, client_ts, idempotency_key) \
         VALUES ($1, $2, 'dismiss', now(), $3)",
    )
    .bind(user.id)
    .bind(dismissed_item)
    .bind(vec![1_u8; 32])
    .fetch_optional(&pool)
    .await
    .unwrap();

    // alice was served twice and clicked once. `impressions` is the penalty's
    // shrinkage denominator and must count impressions only, not every event.
    let alice_item = sqlx::query_scalar::<_, i64>(
        "INSERT INTO recommendation_item (user_id, account_id, kind, score, model_version) \
         VALUES ($1, $2, 'account', 0.9, 0) RETURNING id",
    )
    .bind(user.id)
    .bind(alice)
    .fetch_one(&pool)
    .await
    .unwrap();
    for (kind, key) in [("impression", 10_u8), ("impression", 11), ("click", 12)] {
        sqlx::query(
            "INSERT INTO interaction_event (user_id, item_id, kind, client_ts, idempotency_key) \
             VALUES ($1, $2, $3, now(), $4)",
        )
        .bind(user.id)
        .bind(alice_item)
        .bind(kind)
        .bind(vec![key; 32])
        .fetch_optional(&pool)
        .await
        .unwrap();
    }

    for (account, affinity, expired) in [
        (alice, 0.9_f32, false),
        (bob, 0.5, false),
        (carol, 0.95, false),
        (dave, 0.4, false),
        (erin, 0.99, true),
    ] {
        sqlx::query(
            "INSERT INTO account_affinity (user_id, account_id, affinity, support, sources, expires_at) \
             VALUES ($1, $2, $3, 2, ARRAY['similar_user']::text[], \
                 now() + CASE WHEN $4 THEN interval '-1 hour' ELSE interval '1 day' END)",
        )
        .bind(user.id)
        .bind(account)
        .bind(affinity)
        .bind(expired)
        .fetch_optional(&pool)
        .await
        .unwrap();
    }

    let candidates = feed::candidates(&pool, user.id, 10).await.unwrap();
    let accts: Vec<&str> = candidates.iter().map(|c| c.acct.as_str()).collect();
    assert_eq!(
        accts,
        vec!["alice", "bob", "dave"],
        "unexpected candidates: {accts:?}"
    );
    assert_eq!(candidates[0].support, 2);
    assert_eq!(candidates[0].instance_host, "remote.test");
    assert_eq!(
        candidates[0].impressions, 2,
        "only impression events feed the shrinkage prior"
    );
    assert!(candidates[0].rejected_kind.is_none());

    let dave_row = candidates
        .iter()
        .find(|candidate| candidate.acct == "dave")
        .expect("a dismissed account is still a candidate");
    assert_eq!(dave_row.rejected_kind.as_deref(), Some("dismiss"));
    assert!(dave_row.rejected_at.is_some());

    // A limit smaller than the candidate set must come back truncated, not extended.
    let limited = feed::candidates(&pool, user.id, 1).await.unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].acct, "alice");
}

#[sqlx::test(migrations = "./migrations")]
async fn a_snapshot_is_read_back_by_cursor(pool: PgPool) {
    users::ensure_instance(&pool, "home.test").await.unwrap();
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    let user = users::upsert(&pool, "home.test", 1, "me").await.unwrap();
    accounts::upsert_many(
        &pool,
        &[
            new_account("remote.test", 10, "alice"),
            new_account("remote.test", 11, "bob"),
        ],
    )
    .await
    .unwrap();
    let alice = accounts::find_id(&pool, "remote.test", 10)
        .await
        .unwrap()
        .unwrap();
    let bob = accounts::find_id(&pool, "remote.test", 11)
        .await
        .unwrap()
        .unwrap();

    let feed_id = Uuid::now_v7();
    let reason = serde_json::json!({"code": "similar_user", "support": 2});
    let written = feed::persist(
        &pool,
        user.id,
        feed_id,
        0,
        &[
            NewItem {
                account_id: alice,
                score: 0.9,
                reason: reason.clone(),
            },
            NewItem {
                account_id: bob,
                score: 0.5,
                reason,
            },
        ],
    )
    .await
    .unwrap();
    assert_eq!(written.len(), 2);

    let first = feed::page(&pool, user.id, feed_id, 0, 1).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].id, written[0].0);
    assert_eq!(first[0].acct, "alice");

    let second = feed::page(&pool, user.id, feed_id, first[0].id, 10)
        .await
        .unwrap();
    assert_eq!(second.len(), 1, "the cursor must not repeat an item");
    assert_eq!(second[0].acct, "bob");
    assert_eq!(second[0].reason["support"], 2);

    // Another user's snapshot is not visible to this one.
    let stranger = users::upsert(&pool, "home.test", 2, "other").await.unwrap();
    assert!(
        feed::page(&pool, stranger.id, feed_id, 0, 10)
            .await
            .unwrap()
            .is_empty()
    );

    assert_eq!(feed::active_model_version(&pool).await.unwrap(), 0);
    assert_eq!(
        feed::similarity_age_hours(&pool, user.id).await.unwrap(),
        None
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_snapshot_has_a_reuse_window_and_a_lifetime(pool: PgPool) {
    users::ensure_instance(&pool, "home.test").await.unwrap();
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    let user = users::upsert(&pool, "home.test", 1, "me").await.unwrap();
    accounts::upsert_many(&pool, &[new_account("remote.test", 10, "alice")])
        .await
        .unwrap();
    let alice = accounts::find_id(&pool, "remote.test", 10)
        .await
        .unwrap()
        .unwrap();

    let feed_id = Uuid::now_v7();
    feed::persist(
        &pool,
        user.id,
        feed_id,
        0,
        &[NewItem {
            account_id: alice,
            score: 0.9,
            reason: serde_json::json!({"code": "similar_user"}),
        }],
    )
    .await
    .unwrap();

    // Freshly written: reusable for any window, and younger than a day.
    let reused = feed::live_snapshot(&pool, user.id, minutes(5))
        .await
        .unwrap();
    assert_eq!(reused.map(|(id, _)| id), Some(feed_id));
    let age = feed::snapshot_age(&pool, user.id, feed_id)
        .await
        .unwrap()
        .expect("a snapshot that exists has an age");
    assert!(
        age < 60.0,
        "a fresh snapshot is seconds old, not hours: {age}"
    );

    // Backdate it: still readable by cursor, but not reusable, and past its lifetime.
    sqlx::query(
        "UPDATE recommendation_item SET served_at = now() - interval '30 hours' \
         WHERE user_id = $1",
    )
    .bind(user.id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        feed::live_snapshot(&pool, user.id, minutes(10))
            .await
            .unwrap()
            .is_none(),
        "a thirty-hour-old snapshot is outside the reuse window"
    );
    let age = feed::snapshot_age(&pool, user.id, feed_id)
        .await
        .unwrap()
        .expect("the rows are still there");
    assert!(
        age > 24.0 * 3600.0,
        "thirty hours is more than a day: {age}"
    );

    // A feed id nobody minted has no age at all -- which the route must not confuse
    // with "expired": both restart the client, but only one can be explained.
    assert!(
        feed::snapshot_age(&pool, user.id, Uuid::now_v7())
            .await
            .unwrap()
            .is_none()
    );
}

fn minutes(count: i64) -> sqlx::postgres::types::PgInterval {
    sqlx::postgres::types::PgInterval {
        months: 0,
        days: 0,
        microseconds: count * 60 * 1_000_000,
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn popular_candidates_exclude_followed_and_order_by_followers(pool: PgPool) {
    users::ensure_instance(&pool, "home.test").await.unwrap();
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    let user = users::upsert(&pool, "home.test", 1, "me").await.unwrap();

    // Four accounts, each with a distinct follower count, so the order the query
    // returns is the order the counts imply and not an accident of insertion.
    accounts::upsert_many(
        &pool,
        &[
            new_account("home.test", 1, "me"),
            NewAccount {
                followers_count: 400,
                ..new_account("remote.test", 10, "alice")
            },
            NewAccount {
                followers_count: 300,
                ..new_account("remote.test", 11, "bob")
            },
            NewAccount {
                followers_count: 200,
                ..new_account("remote.test", 12, "carol")
            },
            NewAccount {
                followers_count: 100,
                ..new_account("remote.test", 13, "dave")
            },
        ],
    )
    .await
    .unwrap();

    let me = accounts::find_id(&pool, "home.test", 1)
        .await
        .unwrap()
        .unwrap();
    let alice = accounts::find_id(&pool, "remote.test", 10)
        .await
        .unwrap()
        .unwrap();
    let bob = accounts::find_id(&pool, "remote.test", 11)
        .await
        .unwrap()
        .unwrap();

    // alice is the most popular and already followed, bob is next and was dismissed:
    // both are excluded even though a bare `ORDER BY followers_count` would rank them
    // first.
    accounts::replace_follows(&pool, me, &[alice], "server_sync")
        .await
        .unwrap();

    let dismissed_item = sqlx::query_scalar::<_, i64>(
        "INSERT INTO recommendation_item (user_id, account_id, kind, score, model_version) \
         VALUES ($1, $2, 'account', 0.4, 0) RETURNING id",
    )
    .bind(user.id)
    .bind(bob)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO interaction_event (user_id, item_id, kind, client_ts, idempotency_key) \
         VALUES ($1, $2, 'dismiss', now(), $3)",
    )
    .bind(user.id)
    .bind(dismissed_item)
    .bind(vec![2_u8; 32])
    .fetch_optional(&pool)
    .await
    .unwrap();

    let popular = feed::popular_candidates(&pool, user.id, 10).await.unwrap();
    let accts: Vec<&str> = popular.iter().map(|c| c.acct.as_str()).collect();
    assert_eq!(
        accts,
        vec!["carol", "dave"],
        "unexpected popular candidates: {accts:?}"
    );

    // The fallback rung carries the neutral affinity and the empty signals, so the
    // ranking pass and the stored reason cannot mistake it for a calculated row.
    assert!((popular[0].affinity - 1.0).abs() < f64::EPSILON);
    assert_eq!(popular[0].support, 0);
    assert_eq!(popular[0].sources, Vec::<String>::new());
    assert_eq!(popular[0].seen_count_7d, 0);
    assert!(popular[0].last_shown_at.is_none());
    assert!(popular[0].rejected_kind.is_none());
    assert!(popular[0].rejected_at.is_none());
    assert_eq!(popular[0].impressions, 0);
    assert_eq!(popular[0].followers_count, 200);

    let limited = feed::popular_candidates(&pool, user.id, 1).await.unwrap();
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].acct, "carol");
}
