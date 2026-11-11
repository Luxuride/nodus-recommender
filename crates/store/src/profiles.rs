//! Profiles: what the recommender compares, for our users and for strangers.
//!
//! A profile is a set of weighted edges, not a person. Our own users get one from the
//! token they granted (`source = 'server_sync'`); any other account gets one from what
//! its instance makes public (`'public_follow'` when the account has not set
//! `hide_collections`, `'public_boost'` from its own boosts, `'public_reply'` from
//! public replies). That is why similarity here is keyed on `account.id` and not on
//! `app_user.id`: the neighbours of a user are *accounts*, most of which are not our
//! users at all.
//!
//! Two rules keep this affordable, and both live in SQL rather than in a caller that
//! could forget them:
//!
//! * [`needing_graph`] only returns accounts somebody actually needs — followed by a user
//!   we serve, or already carrying affinity for one — so the crawler never walks the
//!   fediverse breadth-first.
//! * [`peers_sharing`] only returns accounts that share at least one follow with the
//!   subject, so a similarity pass costs one indexed scan instead of a cartesian product.

use std::fmt;

use sqlx::PgPool;

/// One edge of a profile.
///
/// The three kinds are the signals [`crate::similarity`]'s weights were chosen for:
/// a follow is a durable opinion, a boost a topical one, a reply an argument someone
/// chose to have in public.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    /// Who the account follows.
    Follow,
    /// Whose statuses the account boosted.
    Boost,
    /// Whose statuses the account replied to.
    Reply,
}

impl EdgeKind {
    /// The value stored in `profile_edge.kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Follow => "follow",
            Self::Boost => "boost",
            Self::Reply => "reply",
        }
    }

    /// Parses a stored kind; `None` for anything the schema would reject anyway.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "follow" => Some(Self::Follow),
            "boost" => Some(Self::Boost),
            "reply" => Some(Self::Reply),
            _ => None,
        }
    }
}

impl fmt::Display for EdgeKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Where a profile's edges came from.
///
/// Kept on every row because a crawled follow list and a granted one are not equally
/// trustworthy: ours is complete, a crawled one is whatever the instance exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeSource {
    /// Read with a token the account holder granted.
    ServerSync,
    /// A follow list the instance published.
    PublicFollow,
    /// A boost, visible because the status is.
    PublicBoost,
    /// A public reply.
    PublicReply,
}

impl EdgeSource {
    /// The value stored in `profile_edge.source`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ServerSync => "server_sync",
            Self::PublicFollow => "public_follow",
            Self::PublicBoost => "public_boost",
            Self::PublicReply => "public_reply",
        }
    }
}

/// An account whose graph nobody has crawled yet, or whose crawl is stale.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CrawlTarget {
    /// Surrogate id every other table uses.
    pub id: i64,
    /// The instance that hosts it.
    pub instance_host: String,
    /// The instance's own id for it.
    pub mastodon_id: i64,
}

/// Writes edges for one account, replacing what the same source saw before.
///
/// Replacement, not accumulation: a follow list is a snapshot, and an account that
/// stopped following someone must stop contributing that edge. Edges from *other*
/// sources are untouched, so a later crawl cannot erase what a token told us.
///
/// # Errors
///
/// Propagates [`sqlx::Error`]; a target id that does not exist is a foreign-key
/// violation and means the caller resolved ids from a page it did not upsert.
pub async fn replace_edges(
    pool: &PgPool,
    account_id: i64,
    source: EdgeSource,
    edges: &[(EdgeKind, i64)],
) -> Result<u64, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("DELETE FROM profile_edge WHERE account_id = $1 AND source = $2")
        .bind(account_id)
        .bind(source.as_str())
        .execute(&mut *transaction)
        .await?;

    if edges.is_empty() {
        transaction.commit().await?;
        return Ok(0);
    }

    let kinds: Vec<&str> = edges.iter().map(|(kind, _)| kind.as_str()).collect();
    let targets: Vec<i64> = edges.iter().map(|(_, target)| *target).collect();
    let inserted = sqlx::query(
        "INSERT INTO profile_edge (account_id, kind, target_account_id, source) \
         SELECT $1, kind, target, $2 \
         FROM UNNEST($3::text[], $4::bigint[]) AS page (kind, target) \
         ON CONFLICT (account_id, kind, target_account_id) DO UPDATE \
         SET source = EXCLUDED.source, observed_at = now()",
    )
    .bind(account_id)
    .bind(source.as_str())
    .bind(&kinds)
    .bind(&targets)
    .execute(&mut *transaction)
    .await?
    .rows_affected();

    transaction.commit().await?;
    Ok(inserted)
}

/// This account's edges of one kind, as target ids.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn targets(
    pool: &PgPool,
    account_id: i64,
    kind: EdgeKind,
) -> Result<Vec<i64>, sqlx::Error> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT target_account_id FROM profile_edge \
         WHERE account_id = $1 AND kind = $2 ORDER BY target_account_id",
    )
    .bind(account_id)
    .bind(kind.as_str())
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Records that the graph was crawled, even when it turned out to be private.
///
/// A `hide_collections` account is a permanent answer, not a retry: without this the
/// crawler would pick it again on every pass and spend a request to learn nothing.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn mark_fetched(pool: &PgPool, account_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE account SET graph_fetched_at = now() WHERE id = $1")
        .bind(account_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Accounts whose graph is worth a request right now.
///
/// "Worth it" means somebody we serve has a stake: the account is followed by one of
/// our users, or it already carries affinity for one. The most-audienced come first,
/// and `graph_fetched_at` is the TTL — a private account is skipped after the first
/// attempt because [`mark_fetched`] still ran.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn needing_graph(pool: &PgPool, limit: i64) -> Result<Vec<CrawlTarget>, sqlx::Error> {
    sqlx::query_as::<_, CrawlTarget>(
        "SELECT DISTINCT account.id, account.instance_host, account.mastodon_id \
         FROM account \
         WHERE account.follows_private = false \
           AND account.moved_to_account_id IS NULL \
           AND (account.graph_fetched_at IS NULL \
                OR account.graph_fetched_at < now() - interval '7 days') \
           AND (EXISTS (SELECT 1 FROM follow_edge AS edge WHERE edge.followee_id = account.id) \
                OR EXISTS (SELECT 1 FROM account_affinity AS affinity \
                           WHERE affinity.account_id = account.id) \
                OR EXISTS ( \
                    SELECT 1 FROM profile_edge AS edge \
                    WHERE edge.kind = 'follow' AND edge.target_account_id = account.id \
                )) \
         ORDER BY account.id \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Accounts that share at least one follow edge with the subject.
///
/// This is the neighbour candidate set; a similarity score is only computed for these,
/// so the pass is one index scan over `profile_edge (target_account_id, kind)`.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn peers_sharing(
    pool: &PgPool,
    account_id: i64,
    limit: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        "SELECT peer.account_id \
         FROM profile_edge AS mine \
         JOIN profile_edge AS peer \
           ON peer.target_account_id = mine.target_account_id \
          AND peer.kind = mine.kind \
         WHERE mine.account_id = $1 AND peer.account_id <> $1 \
         GROUP BY peer.account_id \
         ORDER BY count(*) DESC, peer.account_id \
         LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// One row of a computed similarity, before storage.
#[derive(Debug, Clone, Copy)]
pub struct SimilarityRow {
    /// The other account.
    pub peer_id: i64,
    /// The blended score.
    pub score: f32,
    /// Cosine over follows.
    pub co_follow: f32,
    /// Cosine over boosts.
    pub co_boost: f32,
    /// Cosine over replies.
    pub co_reply: f32,
}

/// Replaces the subject's similarity rows in one statement.
///
/// Nothing else reads the old rows once the new ones exist, and a delete-then-insert
/// inside one transaction keeps a reader from seeing half a rewrite.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn replace_similarity(
    pool: &PgPool,
    account_id: i64,
    rows: &[SimilarityRow],
) -> Result<u64, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("DELETE FROM profile_similarity WHERE account_id = $1")
        .bind(account_id)
        .execute(&mut *transaction)
        .await?;

    if rows.is_empty() {
        transaction.commit().await?;
        return Ok(0);
    }

    let peers: Vec<i64> = rows.iter().map(|row| row.peer_id).collect();
    let scores: Vec<f32> = rows.iter().map(|row| row.score).collect();
    let follows: Vec<f32> = rows.iter().map(|row| row.co_follow).collect();
    let boosts: Vec<f32> = rows.iter().map(|row| row.co_boost).collect();
    let replies: Vec<f32> = rows.iter().map(|row| row.co_reply).collect();

    let inserted = sqlx::query(
        "INSERT INTO profile_similarity (account_id, peer_id, score, co_follow, co_boost, co_reply) \
         SELECT $1, peer, score, co_follow, co_boost, co_reply \
         FROM UNNEST($2::bigint[], $3::real[], $4::real[], $5::real[], $6::real[]) \
              AS page (peer, score, co_follow, co_boost, co_reply)",
    )
    .bind(account_id)
    .bind(&peers)
    .bind(&scores)
    .bind(&follows)
    .bind(&boosts)
    .bind(&replies)
    .execute(&mut *transaction)
    .await?
    .rows_affected();

    transaction.commit().await?;
    Ok(inserted)
}

/// The subject's nearest accounts, highest score first.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn similar_pairs(
    pool: &PgPool,
    account_id: i64,
    limit: i64,
) -> Result<Vec<(i64, f32)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, f32)>(
        "SELECT peer_id, score FROM profile_similarity \
         WHERE account_id = $1 ORDER BY score DESC, peer_id LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// The app user who holds this account, when it is one of ours.
///
/// A crawled account has no user behind it, and that is normal — it can still be a
/// neighbour for ours.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn owner(pool: &PgPool, account_id: i64) -> Result<Option<uuid::Uuid>, sqlx::Error> {
    // `app_user` identifies an account the way the instance does — host plus its own id —
    // because it exists before the account row does (a user can sign in with a token
    // before any of their follows are crawled).
    let row: Option<(uuid::Uuid,)> = sqlx::query_as(
        "SELECT app_user.id \
         FROM app_user \
         JOIN account \
           ON account.instance_host = app_user.instance_host \
          AND account.mastodon_id = app_user.mastodon_account_id \
         WHERE account.id = $1 AND app_user.deleted_at IS NULL",
    )
    .bind(account_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}
