//! Reads and writes for [`Account`] and the two edge tables.
//!
//! The account table is the biggest thing we store (see the size budget in
//! `docs/03-data-model.md`), so writes are batched through `UNNEST`: one statement per
//! page of an instance response instead of one per row. That keeps the round-trip
//! count independent of how many accounts a page contains.

use crate::models::{Account, NewAccount};
use sqlx::PgPool;

/// Columns of [`Account`], with `citext` narrowed to `text` for sqlx.
const ACCOUNT_COLUMNS: &str = "id, instance_host::text AS instance_host, mastodon_id, remote_id, \
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
    let remote_ids: Vec<Option<&str>> = accounts.iter().map(|a| a.remote_id.as_deref()).collect();
    let accts: Vec<String> = accounts.iter().map(|a| a.acct.clone()).collect();
    let names: Vec<Option<String>> = accounts.iter().map(|a| a.display_name.clone()).collect();
    let followers: Vec<i32> = accounts.iter().map(|a| a.followers_count).collect();
    let bots: Vec<bool> = accounts.iter().map(|a| a.bot).collect();
    let locked: Vec<bool> = accounts.iter().map(|a| a.locked).collect();
    let discoverable: Vec<Option<bool>> = accounts.iter().map(|a| a.discoverable).collect();
    let indexable: Vec<Option<bool>> = accounts.iter().map(|a| a.indexable).collect();
    let noindex: Vec<Option<bool>> = accounts.iter().map(|a| a.noindex).collect();

    let result = sqlx::query(
        "INSERT INTO account (instance_host, mastodon_id, remote_id, acct, display_name, \
             followers_count, bot, locked, last_fetched_at, discoverable, indexable, noindex) \
         SELECT host, mastodon_id, remote_id, acct, display_name, followers_count, bot, locked, \
                NULL, discoverable, indexable, noindex \
         FROM UNNEST($1::text[], $2::bigint[], $3::text[], $4::text[], $5::text[], $6::int[], \
                     $7::bool[], $8::bool[], $9::bool[], $10::bool[], $11::bool[]) \
             AS page (host, mastodon_id, remote_id, acct, display_name, followers_count, bot, \
                      locked, discoverable, indexable, noindex) \
         ON CONFLICT (instance_host, mastodon_id) DO UPDATE SET \
             remote_id = COALESCE(EXCLUDED.remote_id, account.remote_id), \
             acct = EXCLUDED.acct, \
             display_name = COALESCE(EXCLUDED.display_name, account.display_name), \
             followers_count = EXCLUDED.followers_count, \
             bot = EXCLUDED.bot, \
             locked = EXCLUDED.locked, \
             discoverable = COALESCE(EXCLUDED.discoverable, account.discoverable), \
             indexable = COALESCE(EXCLUDED.indexable, account.indexable), \
             noindex = COALESCE(EXCLUDED.noindex, account.noindex), \
             updated_at = now()",
    )
    .bind(&hosts)
    .bind(&ids)
    .bind(&remote_ids)
    .bind(&accts)
    .bind(&names)
    .bind(&followers)
    .bind(&bots)
    .bind(&locked)
    .bind(&discoverable)
    .bind(&indexable)
    .bind(&noindex)
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

/// Accounts whose recent posts are due for a refresh.
///
/// Only accounts someone follows, or that earned affinity, are worth spending a
/// request on. `last_fetched_at` is stamped by the status fetch, not by the follow
/// graph write, so a newly discovered author is due immediately.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn needing_status_refresh(pool: &PgPool, limit: i64) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT a.id \
           FROM account a \
          WHERE (a.last_fetched_at IS NULL OR a.last_fetched_at < now() - interval '7 days') \
            AND a.moved_to_account_id IS NULL \
            AND (EXISTS (SELECT 1 FROM follow_edge f WHERE f.followee_id = a.id) \
                 OR EXISTS (SELECT 1 FROM account_affinity aa WHERE aa.account_id = a.id) \
                 OR EXISTS ( \
                     SELECT 1 \
                     FROM follow_edge first_hop \
                     JOIN profile_edge second_hop \
                       ON second_hop.account_id = first_hop.followee_id \
                      AND second_hop.kind = 'follow' \
                     JOIN app_user u \
                       ON u.instance_host = a.instance_host \
                     JOIN account me \
                       ON me.instance_host = u.instance_host \
                      AND me.mastodon_id = u.mastodon_account_id \
                     WHERE second_hop.target_account_id = a.id \
                       AND first_hop.follower_id = me.id \
                 ) \
                 OR EXISTS ( \
                     SELECT 1 \
                     FROM profile_edge pe \
                     JOIN follow_edge f ON f.followee_id = pe.account_id \
                     JOIN app_user u ON u.instance_host = a.instance_host \
                     JOIN account me \
                       ON me.instance_host = u.instance_host \
                      AND me.mastodon_id = u.mastodon_account_id \
                     WHERE pe.target_account_id = a.id \
                       AND pe.kind = 'follow' \
                       AND f.follower_id = me.id \
                 )) \
          ORDER BY a.last_fetched_at NULLS FIRST, a.followers_count DESC, a.id \
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
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

/// Store the avatar an account currently advertises.
///
/// Public statuses carry the author's avatar, so the status sweep can keep the
/// account row current without a second account request. An empty value is ignored:
/// a partial instance response must not erase a known avatar.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
/// Remember the id the account has on its own instance.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn set_remote_id(
    pool: &PgPool,
    account_id: i64,
    remote_id: &str,
) -> Result<(), sqlx::Error> {
    if remote_id.trim().is_empty() {
        return Ok(());
    }
    sqlx::query("UPDATE account SET remote_id = $2, updated_at = now() WHERE id = $1 AND remote_id IS DISTINCT FROM $2")
        .bind(account_id)
        .bind(remote_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_avatar(
    pool: &PgPool,
    account_id: i64,
    avatar_url: &str,
) -> Result<(), sqlx::Error> {
    if avatar_url.trim().is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE account SET avatar_url = $2, updated_at = now() \
         WHERE id = $1 AND coalesce(avatar_url, '') <> $2",
    )
    .bind(account_id)
    .bind(avatar_url)
    .execute(pool)
    .await?;
    Ok(())
}

/// Resolve local ids for a batch of instance accounts.
///
/// The crawl upserts accounts and then needs their local ids for `follow_edge`; one
/// query for the batch rather than one per account keeps a 5 000-follow sync to a
/// couple of round trips per page. Ids that are not in the table are simply absent
/// from the result, which callers treat as "the upsert did not land" — a bug worth
/// seeing rather than silently skipping.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn resolve_ids(
    pool: &PgPool,
    instance_host: &str,
    mastodon_ids: &[i64],
) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    if mastodon_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT id, mastodon_id FROM account \
         WHERE instance_host = $1 AND mastodon_id = ANY($2)",
    )
    .bind(instance_host)
    .bind(mastodon_ids)
    .fetch_all(pool)
    .await
}
