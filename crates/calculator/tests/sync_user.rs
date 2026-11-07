//! End-to-end test of `sync_user` against a mock instance and a real database.
//!
//! The mock answers what a Mastodon instance answers — `verify_credentials` and a
//! paginated `following` list — so the test covers the whole path the production
//! job takes: sealed token → transport → upsert → edges → follow-up job. The only
//! thing stubbed is the instance itself, which is exactly the boundary we cannot
//! own.

use std::sync::Arc;

use nodus_calculator::handlers::{Handler, JobError, SyncConfig, SyncUser};
use nodus_store::crypto::TokenSealer;
use nodus_store::{accounts, jobs, users};
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A 32-byte key, base64-encoded, as a deployment would set it.
const KEY_B64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

fn sealer() -> Arc<TokenSealer> {
    Arc::new(TokenSealer::from_base64(KEY_B64).expect("the test key is valid"))
}

fn account_json(id: i64, acct: &str, followers: u32) -> serde_json::Value {
    serde_json::json!({
        "id": id.to_string(),
        "acct": acct,
        "display_name": format!("Person {id}"),
        "note": "",
        "avatar": "",
        "header": "",
        "bot": false,
        "locked": false,
        "followers_count": followers,
        "following_count": 1,
        "statuses_count": 3,
        "last_status_at": null,
    })
}

/// A mirror added to the pool, to prove the test can seed rows directly.
async fn seed_user(pool: &PgPool, host: &str, mastodon_id: i64, token: &str) -> Uuid {
    let sealer = sealer();
    let envelope = sealer.seal(token).expect("sealing works");
    users::ensure_instance(pool, host)
        .await
        .expect("instance row");
    let user = users::upsert(pool, host, mastodon_id, "me")
        .await
        .expect("user row");

    users::set_sync_token(
        pool,
        user.id,
        &envelope,
        &["read:follows".to_owned()],
        &nodus_store::crypto::fingerprint(token),
    )
    .await
    .expect("token stored");
    user.id
}

#[sqlx::test(migrations = "../store/migrations")]
async fn sync_user_walks_every_page_and_replaces_edges(pool: PgPool) {
    let server = MockServer::start().await;
    let host = server.uri().trim_start_matches("http://").to_owned();
    let user_id = seed_user(&pool, &host, 900, "token-abc").await;

    // `verify_credentials` is only called because the user has no account row yet.
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts/verify_credentials"))
        .and(header("authorization", "Bearer token-abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(account_json(900, "me", 12)))
        .mount(&server)
        .await;

    // Two pages: the first is full (so the crawl continues), the second is short.
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts/900/following"))
        .and(query_param("limit", "2"))
        .and(header("authorization", "Bearer token-abc"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            account_json(1, "one", 100),
            account_json(2, "two", 200),
        ])))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts/900/following"))
        .and(query_param("max_id", "2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([account_json(3, "three", 300),])),
        )
        .mount(&server)
        .await;

    let handler = SyncUser::new(
        pool.clone(),
        SyncConfig::new(sealer(), 1_000.0)
            .with_scheme("http")
            .with_page_size(2),
    );
    let job = jobs::enqueue(&pool, &jobs::NewJob::sync_user(&user_id))
        .await
        .expect("enqueue")
        .expect("no outstanding duplicate");
    let claimed = jobs::claim(&pool, "test-worker", 1).await.expect("claim");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].id, job);

    handler.run(&claimed[0]).await.expect("the crawl succeeds");

    let own = accounts::find_id(&pool, &host, 900)
        .await
        .expect("lookup")
        .expect("own account exists");
    let follows = accounts::follows_of(&pool, own).await.expect("edges");
    assert_eq!(follows.len(), 3, "three followed accounts, from two pages");

    // The accounts the crawl touched carry the instance's counts, not zeros.
    let two = accounts::find_id(&pool, &host, 2)
        .await
        .expect("lookup")
        .expect("account two exists");
    let two = accounts::get(&pool, two)
        .await
        .expect("fetch")
        .expect("account two");
    assert_eq!(two.followers_count, 200);
    assert_eq!(two.display_name.as_deref(), Some("Person 2"));

    // A second run replaces edges rather than accumulating them: unfollowing is
    // represented by absence from the next snapshot.
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts/900/following"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([account_json(1, "one", 100),])),
        )
        .mount(&server)
        .await;
    handler
        .run(&claimed[0])
        .await
        .expect("the second crawl succeeds");
    let follows = accounts::follows_of(&pool, own).await.expect("edges");
    assert_eq!(follows.len(), 1, "edges are replaced, not merged");

    // And the follow-up recompute is queued for the tier allowed to take the lock.
    // It is `recompute_similarity`, not a second kind: that pass writes both
    // `user_similarity` and the `account_affinity` the feed reads.
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM job WHERE kind = 'recompute_similarity' AND status = 'pending'",
    )
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(queued, 1);
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_revoked_token_is_permanent_and_clears_the_copy(pool: PgPool) {
    let server = MockServer::start().await;
    let host = server.uri().trim_start_matches("http://").to_owned();
    let user_id = seed_user(&pool, &host, 901, "revoked").await;

    Mock::given(method("GET"))
        .and(path("/api/v1/accounts/901/following"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "error": "The access token is invalid"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts/verify_credentials"))
        .respond_with(ResponseTemplate::new(200).set_body_json(account_json(901, "me", 1)))
        .mount(&server)
        .await;

    let handler = SyncUser::new(
        pool.clone(),
        SyncConfig::new(sealer(), 1_000.0).with_scheme("http"),
    );
    jobs::enqueue(&pool, &jobs::NewJob::sync_user(&user_id))
        .await
        .expect("enqueue");
    let claimed = jobs::claim(&pool, "test-worker", 1).await.expect("claim");

    let error = handler
        .run(&claimed[0])
        .await
        .expect_err("a 401 must not succeed");
    assert!(
        matches!(error, JobError::Permanent(_)),
        "a revocation must not be retried: {error:?}"
    );

    let material = users::sync_material(&pool, user_id).await.expect("lookup");
    assert!(
        material.is_none(),
        "the encrypted token must not survive its revocation"
    );
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_missing_scope_stops_before_the_first_request(pool: PgPool) {
    let server = MockServer::start().await;
    let host = server.uri().trim_start_matches("http://").to_owned();
    let sealer = sealer();
    let envelope = sealer.seal("token").expect("sealing works");
    users::ensure_instance(&pool, &host)
        .await
        .expect("instance");
    let user = users::upsert(&pool, &host, 902, "me").await.expect("user");
    users::set_sync_token(
        &pool,
        user.id,
        &envelope,
        &["read:accounts".to_owned()],
        &nodus_store::crypto::fingerprint("token"),
    )
    .await
    .expect("stored");

    let handler = SyncUser::new(
        pool.clone(),
        SyncConfig::new(sealer, 1_000.0).with_scheme("http"),
    );
    jobs::enqueue(&pool, &jobs::NewJob::sync_user(&user.id))
        .await
        .expect("enqueue");
    let claimed = jobs::claim(&pool, "test-worker", 1).await.expect("claim");
    let error = handler.run(&claimed[0]).await.expect_err("no scope");

    assert!(matches!(error, JobError::Permanent(_)));
    assert!(error.message().contains("read:follows"));
    assert_eq!(
        server.received_requests().await.expect("requests").len(),
        0,
        "consent problems must not spend instance budget"
    );
}
