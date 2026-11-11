//! Integration tests for the per-reader dwell baseline.
//!
//! The baseline is the median of the reader's own measured impressions: the ranking
//! expresses fatigue in units of it, so an account that is skimmed past must weigh less
//! than one that is read. Below a floor of samples there is no baseline at all — a
//! median of five impressions would be noise turned into a multiplier.

use nodus_store::accounts;
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
        discoverable: None,
        indexable: None,
        noindex: None,
    }
}

/// Records one impression of `account` for `user` with the given dwell.
async fn impression(pool: &PgPool, user: Uuid, account: i64, dwell_ms: i32) {
    let item = sqlx::query_scalar::<_, i64>(
        "INSERT INTO recommendation_item (user_id, account_id, kind, score, model_version) \
         VALUES ($1, $2, 'account', 0.5, 0) RETURNING id",
    )
    .bind(user)
    .bind(account)
    .fetch_one(pool)
    .await
    .expect("the item row must insert");

    sqlx::query(
        "INSERT INTO interaction_event \
             (user_id, item_id, kind, client_ts, dwell_ms, idempotency_key) \
         VALUES ($1, $2, 'impression', now(), $3, $4)",
    )
    .bind(user)
    .bind(item)
    .bind(dwell_ms)
    .bind(format!("impression-{item}").into_bytes())
    .fetch_optional(pool)
    .await
    .expect("the event row must insert");
}

#[sqlx::test(migrations = "./migrations")]
async fn the_baseline_is_the_median_of_the_readers_own_impressions(pool: PgPool) {
    users::ensure_instance(&pool, "home.test").await.unwrap();
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    let user = users::upsert(&pool, "home.test", 1, "me").await.unwrap();
    accounts::upsert_many(&pool, &[new_account("remote.test", 10, "alice")])
        .await
        .unwrap();
    let alice = accounts::find_id(&pool, "remote.test", 10)
        .await
        .unwrap()
        .expect("alice exists");

    assert_eq!(
        nodus_store::feed::baseline_ms(&pool, user.id)
            .await
            .unwrap(),
        None,
        "no impressions means no baseline, not a default"
    );

    // Nineteen samples is still below the floor.
    for dwell in 1..=19 {
        impression(&pool, user.id, alice, dwell * 1_000).await;
    }
    assert_eq!(
        nodus_store::feed::baseline_ms(&pool, user.id)
            .await
            .unwrap(),
        None,
        "a median of nineteen impressions is noise with a multiplier"
    );

    // The twentieth completes the floor: the median of 1s..20s is 10.5s.
    impression(&pool, user.id, alice, 20_000).await;
    let baseline = nodus_store::feed::baseline_ms(&pool, user.id)
        .await
        .unwrap()
        .expect("twenty samples produce a baseline");
    assert!(
        (baseline - 10_500.0).abs() < 1.0,
        "expected the median of 1s..20s, got {baseline}"
    );

    // Unmeasured impressions are not zero-second reads: they must not drag the median.
    let unmeasured = sqlx::query_scalar::<_, i64>(
        "INSERT INTO recommendation_item (user_id, account_id, kind, score, model_version) \
         VALUES ($1, $2, 'account', 0.5, 0) RETURNING id",
    )
    .bind(user.id)
    .bind(alice)
    .fetch_one(&pool)
    .await
    .expect("the unmeasured item must insert");
    sqlx::query(
        "INSERT INTO interaction_event \
             (user_id, item_id, kind, client_ts, idempotency_key) \
         VALUES ($1, $2, 'impression', now(), $3)",
    )
    .bind(user.id)
    .bind(unmeasured)
    .bind(b"impression-without-dwell".to_vec())
    .fetch_optional(&pool)
    .await
    .expect("the unmeasured event must insert");

    let unchanged = nodus_store::feed::baseline_ms(&pool, user.id)
        .await
        .unwrap()
        .expect("the baseline survives unmeasured impressions");
    assert!(
        (unchanged - 10_500.0).abs() < 1.0,
        "an impression without a dwell must not count as a zero, got {unchanged}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn impressions_older_than_the_window_do_not_count(pool: PgPool) {
    users::ensure_instance(&pool, "home.test").await.unwrap();
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    let user = users::upsert(&pool, "home.test", 1, "me").await.unwrap();
    accounts::upsert_many(&pool, &[new_account("remote.test", 10, "alice")])
        .await
        .unwrap();
    let alice = accounts::find_id(&pool, "remote.test", 10)
        .await
        .unwrap()
        .expect("alice exists");

    for dwell in 1..=25 {
        impression(&pool, user.id, alice, dwell * 1_000).await;
    }
    sqlx::query("UPDATE interaction_event SET server_ts = now() - interval '90 days'")
        .fetch_optional(&pool)
        .await
        .unwrap();

    assert_eq!(
        nodus_store::feed::baseline_ms(&pool, user.id)
            .await
            .unwrap(),
        None,
        "a taste from three months ago is not the reader's baseline today"
    );
}
