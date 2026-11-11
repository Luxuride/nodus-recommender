//! Integration tests for the reference-profile gate.
//!
//! The gate decides which crawled accounts may act as neighbours for the taste hop, and
//! it exists because `expand_follows` stores a *prefix* of a heavy follower's follows:
//! cosine over a prefix measures crawl depth as much as taste. What must hold is that an
//! account enters only when its stored follows are a large enough fraction of its stated
//! `following_count` **and** large enough in absolute terms, that our own users are never
//! treated as references (they are compared through `user_similarity`), and that a pass
//! replaces a user's neighbours instead of accumulating them.

use nodus_store::accounts;
use nodus_store::models::NewAccount;
use nodus_store::reference::{self, ReferenceRow};
use nodus_store::users;
use sqlx::PgPool;

/// An account with `followers` follower count; `following_count` is set separately
/// because it is a claim about the account, not something the upsert knows.
fn account(host: &str, mastodon_id: i64, acct: &str, followers: i32) -> NewAccount {
    NewAccount {
        instance_host: host.to_owned(),
        mastodon_id,
        acct: acct.to_owned(),
        display_name: None,
        followers_count: followers,
        bot: false,
        locked: false,
        discoverable: None,
        indexable: None,
        noindex: None,
    }
}

/// Sets the claim the coverage gate measures against.
async fn claim_following_count(pool: &PgPool, mastodon_id: i64, following_count: i32) {
    sqlx::query("UPDATE account SET following_count = $2 WHERE instance_host = 'remote.test' AND mastodon_id = $1")
        .bind(mastodon_id)
        .bind(following_count)
        .execute(pool)
        .await
        .unwrap();
}

/// Gives `mastodon_id` exactly `count` stored follows, creating the followees.
async fn give_follows(pool: &PgPool, mastodon_id: i64, count: i32) {
    users::ensure_instance(pool, "followee.test").await.unwrap();
    let mut followees = Vec::new();
    for offset in 0..count {
        let id = 10_000 + i64::from(offset);
        followees.push(account("followee.test", id, &format!("f{id}"), 10));
    }
    accounts::upsert_many(pool, &followees).await.unwrap();

    let follower = accounts::find_id(pool, "remote.test", mastodon_id)
        .await
        .unwrap()
        .expect("follower exists");
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM account WHERE instance_host = 'followee.test' ORDER BY id LIMIT $1",
    )
    .bind(i64::from(count))
    .fetch_all(pool)
    .await
    .unwrap();
    accounts::replace_follows(pool, follower, &ids, "server_sync")
        .await
        .unwrap();
}

#[sqlx::test(migrations = "./migrations")]
async fn keeps_only_accounts_whose_follows_are_covered(pool: PgPool) {
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    accounts::upsert_many(
        &pool,
        &[
            account("remote.test", 1, "covered", 100),
            account("remote.test", 2, "half_crawled", 100),
            account("remote.test", 3, "known_too_small", 100),
            account("remote.test", 4, "unknown_total", 100),
        ],
    )
    .await
    .unwrap();

    // 90 of 100 stored: coverage 0.90, admitted.
    give_follows(&pool, 1, 90).await;
    claim_following_count(&pool, 1, 100).await;
    // 50 of 100 stored: coverage 0.50, refused — the missing half is the signal.
    give_follows(&pool, 2, 50).await;
    claim_following_count(&pool, 2, 100).await;
    // 39 stored: ratio would be 1.0 against a claimed 39, but the absolute floor wins.
    give_follows(&pool, 3, 39).await;
    claim_following_count(&pool, 3, 39).await;
    // 90 stored and no claim: coverage is undefined, so there is nothing to trust.
    give_follows(&pool, 4, 90).await;
    claim_following_count(&pool, 4, 0).await;

    let gated = reference::gated(&pool, reference::MAX_REFERENCES)
        .await
        .unwrap();

    assert_eq!(
        gated.len(),
        1,
        "only the covered account qualifies: {gated:?}"
    );
    assert_eq!(gated[0].known_follows, 90);
    assert!(
        (gated[0].coverage - 0.9).abs() < 1e-9,
        "{}",
        gated[0].coverage
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn positive_boundaries_are_inclusive(pool: PgPool) {
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    accounts::upsert_many(
        &pool,
        &[
            account("remote.test", 1, "exactly_the_floor", 100),
            account("remote.test", 2, "exactly_full_coverage", 40),
        ],
    )
    .await
    .unwrap();

    // 40 known follows against a claim of 100: coverage 0.4 fails, so make the claim
    // match the stores: this one tests the absolute floor, not the ratio.
    give_follows(&pool, 1, 40).await;
    claim_following_count(&pool, 1, 40).await;
    give_follows(&pool, 2, 40).await;
    claim_following_count(&pool, 2, 40).await;

    let gated = reference::gated(&pool, reference::MAX_REFERENCES)
        .await
        .unwrap();
    assert_eq!(gated.len(), 2, "both sit exactly on the floor: {gated:?}");
    assert!(gated.iter().all(|reference| reference.coverage >= 1.0));
}

#[sqlx::test(migrations = "./migrations")]
async fn our_own_users_are_never_references(pool: PgPool) {
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    accounts::upsert_many(&pool, &[account("remote.test", 1, "both", 100)])
        .await
        .unwrap();
    give_follows(&pool, 1, 90).await;
    claim_following_count(&pool, 1, 100).await;

    // Before the user exists the account is a candidate.
    assert_eq!(
        reference::gated(&pool, reference::MAX_REFERENCES)
            .await
            .unwrap()
            .len(),
        1
    );

    // Signing up removes it: favourites and replies are available for a user, so the
    // lower-fidelity comparison must not run in parallel and inflate the count.
    users::upsert(&pool, "remote.test", 1, "both")
        .await
        .unwrap();
    assert_eq!(
        reference::gated(&pool, reference::MAX_REFERENCES)
            .await
            .unwrap(),
        vec![]
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_pass_replaces_previous_neighbours(pool: PgPool) {
    users::ensure_instance(&pool, "home.test").await.unwrap();
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    let user = users::upsert(&pool, "home.test", 1, "me").await.unwrap().id;
    accounts::upsert_many(
        &pool,
        &[
            account("remote.test", 1, "first", 100),
            account("remote.test", 2, "second", 100),
        ],
    )
    .await
    .unwrap();
    let first = accounts::find_id(&pool, "remote.test", 1)
        .await
        .unwrap()
        .unwrap();
    let second = accounts::find_id(&pool, "remote.test", 2)
        .await
        .unwrap()
        .unwrap();

    let row = |account_id, similarity: f32| ReferenceRow {
        user_id: user,
        account_id,
        similarity,
        co_follow: similarity,
        coverage: 0.9,
    };

    let written = reference::replace_for_user(&pool, user, &[row(first, 0.5), row(second, 0.7)])
        .await
        .unwrap();
    assert_eq!(written, 2);
    let stored = reference::for_user(&pool, user).await.unwrap();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[0].account_id, second, "strongest first");

    // A smaller pass must not leave the dropped neighbour behind: a stale row is a
    // neighbour the next feed would still use.
    reference::replace_for_user(&pool, user, &[row(first, 0.4)])
        .await
        .unwrap();
    let stored = reference::for_user(&pool, user).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].account_id, first);

    reference::replace_for_user(&pool, user, &[]).await.unwrap();
    assert_eq!(reference::for_user(&pool, user).await.unwrap(), vec![]);
}

#[sqlx::test(migrations = "./migrations")]
async fn follows_are_read_with_the_weights_idf_needs(pool: PgPool) {
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    users::ensure_instance(&pool, "followee.test")
        .await
        .unwrap();
    accounts::upsert_many(
        &pool,
        &[
            account("remote.test", 1, "reader", 0),
            account("followee.test", 10, "popular", 5_000),
            account("followee.test", 11, "niche", 3),
        ],
    )
    .await
    .unwrap();
    let reader = accounts::find_id(&pool, "remote.test", 1)
        .await
        .unwrap()
        .unwrap();
    let popular = accounts::find_id(&pool, "followee.test", 10)
        .await
        .unwrap()
        .unwrap();
    let niche = accounts::find_id(&pool, "followee.test", 11)
        .await
        .unwrap()
        .unwrap();
    accounts::replace_follows(&pool, reader, &[popular, niche], "server_sync")
        .await
        .unwrap();

    let mut follows = reference::follows_of(&pool, reader).await.unwrap();
    follows.sort_by_key(|(id, _)| *id);

    assert_eq!(
        follows,
        vec![(popular, 5_000), (niche, 3)],
        "the follower count travels with the id"
    );
}
