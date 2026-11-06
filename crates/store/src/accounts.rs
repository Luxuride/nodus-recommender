//! Reads and writes for [`Account`] and the two edge tables.
//!
//! The account table is the biggest thing we store (see the size budget in
//! `docs/03-data-model.md`), so writes are batched through `UNNEST`: one statement per
//! page of an instance response instead of one per row. That keeps the round-trip
//! count independent of how many accounts a page contains.

use crate::models::{Account, NewAccount};
use sqlx::PgPool;

/// Columns of [`Account`], with `citext` narrowed to `text` for sqlx.
const ACCOUNT_COLUMNS: &str = "id, instance_host::text AS instance_host, mastodon_id, \
     acct::text AS acct, display_name, followers_count, bot, locked, last_fetched_at, \
     moved_to_account_id";

/// Insert or refresh a batch of accounts.
///
/// Returns the number of rows written. Counts are replaced, never merged: a follower
/// count can fall, and keeping a stale maximum would inflate every popularity score
/// derived from it.
///
/// # Errors
///
/// Propagates [`sqlx::Error`]. The caller must have ensured the `instance` rows exist
/// (see [`crate::users::ensure_instance`]).
pub async fn upsert_many(pool: &PgPool, accounts: &[NewAccount]) -> Result<u64, sqlx::Error> {
    if accounts.is_empty() {
        return Ok(0);
    }
    let hosts: Vec<String> = accounts.iter().map(|a| a.instance_host.clone()).collect();
    let ids: Vec<i64> = accounts.iter().map(|a| a.mastodon_id).collect();
    let accts: Vec<String> = accounts.iter().map(|a| a.acct.clone()).collect();
    let names: Vec<Option<String>> = accounts.iter().map(|a| a.display_name.clone()).collect();
    let followers: Vec<i32> = accounts.iter().map(|a| a.followers_count).collect();
    let bots: Vec<bool> = accounts.iter().map(|a| a.bot).collect();
    let locked: Vec<bool> = accounts.iter().map(|a| a.locked).collect();

    let result = sqlx::query(
        "INSERT INTO account (instance_host, mastodon_id, acct, display_name, followers_count, \
             bot, locked, last_fetched_at) \
         SELECT host, mastodon_id, acct, display_name, followers_count, bot, locked, now() \
         FROM UNNEST($1::text[], $2::bigint[], $3::text[], $4::text[], $5::int[], \
                     $6::bool[], $7::bool[]) \
             AS page (host, mastodon_id, acct, display_name, followers_count, bot, locked) \
         ON CONFLICT (instance_host, mastodon_id) DO UPDATE SET \
             acct = EXCLUDED.acct, \
             display_name = COALESCE(EXCLUDED.display_name, account.display_name), \
             followers_count = EXCLUDED.followers_count, \
             bot = EXCLUDED.bot, \
             locked = EXCLUDED.locked, \
             last_fetched_at = now(), \
             updated_at = now()",
    )
    .bind(&hosts)
    .bind(&ids)
    .bind(&accts)
    .bind(&names)
    .bind(&followers)
    .bind(&bots)
    .bind(&locked)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Fetch one account by its local id.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn get(pool: &PgPool, id: i64) -> Result<Option<Account>, sqlx::Error> {
    let sql = format!("SELECT {ACCOUNT_COLUMNS} FROM account WHERE id = $1");
    sqlx::query_as::<_, Account>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// Resolve the local id of an account from its instance identity.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn find_id(
    pool: &PgPool,
    instance_host: &str,
    mastodon_id: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM account WHERE instance_host = $1 AND mastodon_id = $2",
    )
    .bind(instance_host)
    .bind(mastodon_id)
    .fetch_optional(pool)
    .await
}

/// Replace the follow set of one account.
///
/// Deleting before inserting is what makes an unfollow visible: a follow graph that
/// only ever grows would keep recommending people the user has already dropped. The
/// two statements run in one transaction, and the `source` of an edge is overwritten
/// by the most recent observation so the provenance table stays honest about where a
/// row came from last.
///
/// # Errors
///
/// Propagates [`sqlx::Error`]. Callers pass local account ids, so an unknown id is a
/// foreign-key violation and indicates a bug upstream.
pub async fn replace_follows(
    pool: &PgPool,
    follower_id: i64,
    followee_ids: &[i64],
    source: &str,
) -> Result<u64, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM follow_edge WHERE follower_id = $1")
        .bind(follower_id)
        .execute(&mut *tx)
        .await?;
    let inserted = if followee_ids.is_empty() {
        0
    } else {
        sqlx::query(
            "INSERT INTO follow_edge (follower_id, followee_id, source) \
             SELECT $1, followee_id, $3 FROM UNNEST($2::bigint[]) AS page (followee_id) \
             WHERE followee_id <> $1 \
             ON CONFLICT (follower_id, followee_id) DO UPDATE SET \
                 last_seen_at = now(), source = EXCLUDED.source",
        )
        .bind(follower_id)
        .bind(followee_ids)
        .bind(source)
        .execute(&mut *tx)
        .await?
        .rows_affected()
    };
    tx.commit().await?;
    Ok(inserted)
}

/// The accounts an account follows, as local ids.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn follows_of(pool: &PgPool, follower_id: i64) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT followee_id FROM follow_edge WHERE follower_id = $1")
        .bind(follower_id)
        .fetch_all(pool)
        .await
}

/// Record one engagement observation, accumulating repeats.
///
/// A second favourite of the same target does not create a second edge; it bumps the
/// count. That is the cheapest honest summary of "how much this person engages with
/// that one" without storing every individual event.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn record_engagement(
    pool: &PgPool,
    actor_id: i64,
    target_id: i64,
    kind: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO engagement_edge (actor_id, target_id, kind, count) VALUES ($1, $2, $3, 1) \
         ON CONFLICT (actor_id, target_id, kind) \
         DO UPDATE SET count = engagement_edge.count + 1, last_seen_at = now()",
    )
    .bind(actor_id)
    .bind(target_id)
    .bind(kind)
    .execute(pool)
    .await?;
    Ok(())
}
