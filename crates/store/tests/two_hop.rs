//! Integration tests for the graph hop.
//!
//! The hop is the candidate source that needs no similarity pass, so its correctness is
//! the difference between a feed that fills on a young instance and one that stays empty
//! until strangers overlap. What must hold: candidates arrive through followees, accounts
//! the reader already follows never do, and a bridge the reader has engaged with counts
//! for more than one they have not.

use nodus_store::accounts;
use nodus_store::models::NewAccount;
use nodus_store::two_hop;
use nodus_store::users;
use sqlx::PgPool;

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

/// Resolve a remote account id, which most assertions need.
async fn remote_id(pool: &PgPool, mastodon_id: i64) -> i64 {
    accounts::find_id(pool, "remote.test", mastodon_id)
        .await
        .unwrap()
        .expect("account exists")
}

#[sqlx::test(migrations = "./migrations")]
async fn candidates_arrive_through_followees_and_never_through_the_reader(pool: PgPool) {
    users::ensure_instance(&pool, "home.test").await.unwrap();
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    let user = users::upsert(&pool, "home.test", 1, "me").await.unwrap();

    accounts::upsert_many(
        &pool,
        &[
            account("home.test", 1, "me", 0),
            account("remote.test", 10, "bridge", 100),
            account("remote.test", 11, "followed", 100),
            account("remote.test", 12, "via_bridge", 100),
            account("remote.test", 13, "via_both", 100),
        ],
    )
    .await
    .unwrap();

    let me = accounts::find_id(&pool, "home.test", 1)
        .await
        .unwrap()
        .expect("own account exists");
    let bridge = remote_id(&pool, 10).await;
    let followed = remote_id(&pool, 11).await;
    let via_bridge = remote_id(&pool, 12).await;
    let via_both = remote_id(&pool, 13).await;

    accounts::replace_follows(&pool, me, &[bridge, followed], "server_sync")
        .await
        .unwrap();
    accounts::replace_follows(&pool, bridge, &[via_bridge, via_both], "server_sync")
        .await
        .unwrap();
    accounts::record_engagement(&pool, me, bridge, "favourite")
        .await
        .unwrap();

    let rows = two_hop::candidates(&pool, user.id, 50).await.unwrap();
    let by_id = |account_id: i64| rows.iter().find(|row| row.account_id == account_id);

    assert!(
        by_id(bridge).is_none() && by_id(followed).is_none() && by_id(me).is_none(),
        "an account the reader already follows (or is) is not a candidate: {rows:?}"
    );

    let single = by_id(via_bridge).expect("a followee's followee is a candidate");
    assert_eq!(single.support, 1, "one bridge reaches it");
    assert_eq!(
        single.engaged_paths, 1,
        "the bridge is one the reader engaged with"
    );
    assert!(single.weight > 0.0, "the path carries positive weight");

    let shared = by_id(via_both).expect("reachable candidate");
    assert_eq!(shared.support, 1, "only the engaged bridge follows it here");
}

#[sqlx::test(migrations = "./migrations")]
async fn an_engaged_bridge_weighs_more_than_a_stranger(pool: PgPool) {
    users::ensure_instance(&pool, "home.test").await.unwrap();
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    let user = users::upsert(&pool, "home.test", 1, "me").await.unwrap();

    accounts::upsert_many(
        &pool,
        &[
            account("home.test", 1, "me", 0),
            account("remote.test", 10, "engaged", 100),
            account("remote.test", 11, "stranger", 100),
            account("remote.test", 12, "through_engaged", 100),
            account("remote.test", 13, "through_stranger", 100),
        ],
    )
    .await
    .unwrap();

    let ids = sqlx::query_as::<_, (i64, i64)>(
        "SELECT mastodon_id, id FROM account WHERE instance_host = 'remote.test'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let id = |mastodon_id: i64| {
        ids.iter()
            .find(|(remote, _)| *remote == mastodon_id)
            .map(|(_, local)| *local)
            .expect("account exists")
    };
    let me = accounts::find_id(&pool, "home.test", 1)
        .await
        .unwrap()
        .expect("own account exists");
    let (engaged, stranger) = (id(10), id(11));
    let (through_engaged, through_stranger) = (id(12), id(13));

    accounts::replace_follows(&pool, me, &[engaged, stranger], "server_sync")
        .await
        .unwrap();
    accounts::replace_follows(&pool, engaged, &[through_engaged], "server_sync")
        .await
        .unwrap();
    accounts::replace_follows(&pool, stranger, &[through_stranger], "server_sync")
        .await
        .unwrap();
    accounts::record_engagement(&pool, me, engaged, "reply")
        .await
        .unwrap();

    let rows = two_hop::candidates(&pool, user.id, 50).await.unwrap();
    let weight = |account_id: i64| {
        rows.iter()
            .find(|row| row.account_id == account_id)
            .map(|row| row.weight)
            .expect("candidate present")
    };
    let promoted = weight(through_engaged);
    let damped = weight(through_stranger);
    assert!(
        promoted > damped,
        "engaging with the bridge promotes its path: {promoted} vs {damped}"
    );
    let ratio = promoted / damped;
    assert!(
        (ratio - (two_hop::ENGAGED_BRIDGE_WEIGHT / two_hop::BRIDGE_WEIGHT)).abs() < 0.01,
        "the ratio is exactly the damping ratio, got {ratio}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn support_counts_distinct_bridges(pool: PgPool) {
    users::ensure_instance(&pool, "home.test").await.unwrap();
    users::ensure_instance(&pool, "remote.test").await.unwrap();
    let user = users::upsert(&pool, "home.test", 1, "me").await.unwrap();

    accounts::upsert_many(
        &pool,
        &[
            account("home.test", 1, "me", 0),
            account("remote.test", 10, "one", 100),
            account("remote.test", 11, "two", 100),
            account("remote.test", 12, "popular", 100),
        ],
    )
    .await
    .unwrap();

    let me = accounts::find_id(&pool, "home.test", 1)
        .await
        .unwrap()
        .expect("own account exists");
    let one = remote_id(&pool, 10).await;
    let two = remote_id(&pool, 11).await;
    let popular = remote_id(&pool, 12).await;

    accounts::replace_follows(&pool, me, &[one, two], "server_sync")
        .await
        .unwrap();
    accounts::replace_follows(&pool, one, &[popular], "server_sync")
        .await
        .unwrap();
    accounts::replace_follows(&pool, two, &[popular], "server_sync")
        .await
        .unwrap();

    let rows = two_hop::candidates(&pool, user.id, 50).await.unwrap();
    let found = rows
        .iter()
        .find(|row| row.account_id == popular)
        .expect("a candidate both followees follow");
    assert_eq!(found.support, 2, "two independent bridges, not two edges");
    assert_eq!(found.engaged_paths, 0, "the reader engaged with neither");
}
