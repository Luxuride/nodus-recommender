//! Integration tests for the D23 token pool.
//!
//! The value of `pick_token` is its SQL — the ordering, the partial-index predicate
//! and the single-statement claim — so these run against a real Postgres. A mocked
//! driver would only test that the mock returns what it was told to.

use nodus_store::crypto::fingerprint;
use nodus_store::users::{self, PooledToken};
use sqlx::PgPool;
use uuid::Uuid;

const HOST: &str = "pool.test";
const GRAPH_SCOPES: [&str; 1] = ["read:follows"];

/// A user on [`HOST`] with a readable graph token sealed into `b"sealed"`.
async fn seed(pool: &PgPool, mastodon_id: i64, token: &str) -> Uuid {
    let user = users::upsert(pool, HOST, mastodon_id, &format!("user{mastodon_id}"))
        .await
        .expect("user");
    users::set_sync_token(
        pool,
        user.id,
        b"sealed",
        &GRAPH_SCOPES.map(str::to_owned),
        &fingerprint(token),
    )
    .await
    .expect("stored");
    user.id
}

/// Backdate the token clock by a SQL interval, e.g. `"2 hours"`.
///
/// The interval is a literal from the test, never caller input, so interpolating it
/// keeps the helper short.
async fn stamp(pool: &PgPool, user_id: Uuid, age: &str) {
    let sql =
        format!("UPDATE app_user SET token_last_used_at = now() - interval '{age}' WHERE id = $1");
    sqlx::query(&sql)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("stamped");
}

async fn pick(pool: &PgPool, excluded: &[[u8; 32]]) -> PooledToken {
    users::pick_token(pool, HOST, excluded)
        .await
        .expect("query")
        .expect("a token is available")
}

#[sqlx::test(migrations = "./migrations")]
async fn pick_token_prefers_never_used_then_the_oldest(pool: PgPool) {
    users::ensure_instance(&pool, HOST).await.expect("instance");
    let never = seed(&pool, 1, "never").await;
    let oldest = seed(&pool, 2, "oldest").await;
    let newest = seed(&pool, 3, "newest").await;
    stamp(&pool, oldest, "2 hours").await;
    stamp(&pool, newest, "1 minute").await;

    // The clock starts at NULL, which outranks any timestamp.
    let first = pick(&pool, &[]).await;
    assert_eq!(first.user_id, never);
    assert_eq!(first.mastodon_account_id, 1);
    assert_eq!(first.token_scopes, GRAPH_SCOPES.map(str::to_owned));

    // The first pick stamped its row, so the two-hour-old token is next.
    let second = pick(&pool, &[]).await;
    assert_eq!(second.user_id, oldest);

    let third = pick(&pool, &[]).await;
    assert_eq!(third.user_id, newest);
}

#[sqlx::test(migrations = "./migrations")]
async fn pick_token_skips_excluded_fingerprints(pool: PgPool) {
    users::ensure_instance(&pool, HOST).await.expect("instance");
    seed(&pool, 10, "first").await;
    let second = seed(&pool, 11, "second").await;

    let picked = pick(&pool, &[fingerprint("first")]).await;
    assert_eq!(picked.user_id, second);
    assert_eq!(picked.mastodon_account_id, 11);

    let none = users::pick_token(&pool, HOST, &[fingerprint("first"), fingerprint("second")])
        .await
        .expect("query");
    assert!(none.is_none(), "every candidate was excluded");
}

#[sqlx::test(migrations = "./migrations")]
async fn pick_token_skips_disabled_deleted_and_scopeless_rows(pool: PgPool) {
    users::ensure_instance(&pool, HOST).await.expect("instance");
    let healthy = seed(&pool, 20, "healthy").await;

    let disabled = seed(&pool, 21, "disabled").await;
    users::clear_sync_token(&pool, disabled)
        .await
        .expect("cleared");

    let deleted = seed(&pool, 22, "deleted").await;
    users::soft_delete(&pool, deleted).await.expect("deleted");

    let scopeless = users::upsert(&pool, HOST, 23, "scopeless")
        .await
        .expect("user");
    users::set_sync_token(
        &pool,
        scopeless.id,
        b"sealed",
        &["read:accounts".to_owned()],
        &fingerprint("scopeless"),
    )
    .await
    .expect("stored");

    let picked = pick(&pool, &[]).await;
    assert_eq!(picked.user_id, healthy);
}

#[sqlx::test(migrations = "./migrations")]
async fn two_picks_take_different_tokens(pool: PgPool) {
    users::ensure_instance(&pool, HOST).await.expect("instance");
    let a = seed(&pool, 30, "a").await;
    let b = seed(&pool, 31, "b").await;

    // With both clocks at NULL the tie is broken by `user_id`, so the expected order
    // is the ids sorted.
    let expected = if a < b { (a, b) } else { (b, a) };
    let first = pick(&pool, &[]).await;
    let second = pick(&pool, &[]).await;

    // Different tokens: the `last_used` clock really advanced, so the second call
    // cannot hand out the first one again.
    assert_eq!((first.user_id, second.user_id), expected);

    // And the clock is a timestamp, not a null→null no-op.
    let stamped: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT token_last_used_at FROM app_user WHERE id = $1")
            .bind(first.user_id)
            .fetch_one(&pool)
            .await
            .expect("clock");
    assert!(stamped.is_some());
}
