//! Reads and writes for [`Status`](NewStatus) rows.
//!
//! Status ingestion is deliberately a small surface: one batched upsert per page,
//! one read for the post ranker, and the stamp that tells the refresh sweep the
//! account's posts were just visited. The table is the retention unit described in
//! `docs/04-ingestion.md`, so the write also refreshes `expires_at`; keeping that
//! arithmetic next to the insert means a caller cannot forget it.
//!
//! The `status` table is a cache of public content, not a copy of a person's
//! timeline. The `CHECK` constraint only admits `public`/`unlisted` rows, and
//! [`upsert_many`] filters the same set in Rust so a caller bug produces no rows
//! instead of a constraint violation that poisons the whole page.

use nodus_shared::{PostEmoji, PostMedia};
use serde_json::Value;
use sqlx::PgPool;
use time::OffsetDateTime;

/// One status page entry, normalised for [`upsert_many`].
///
/// `account_id` is the local `account.id`, not the instance's account id: the
/// handler has already resolved the author it fetched the page for.
#[derive(Debug, Clone)]
pub struct NewStatus {
    /// The instance the status was read from.
    pub instance_host: String,
    /// Instance-local status id.
    pub mastodon_id: i64,
    /// Local `account.id` of the author.
    pub account_id: i64,
    /// HTML content, as the instance returned it.
    pub content: String,
    /// `public` or `unlisted`; anything else is dropped before the write.
    pub visibility: String,
    /// Favourite count at fetch time.
    pub favourites_count: i32,
    /// Boost count at fetch time.
    pub reblogs_count: i32,
    /// Reply count at fetch time.
    pub replies_count: i32,
    /// Language code when the instance reported one.
    pub language: Option<String>,
    /// When the status was published.
    pub published_at: OffsetDateTime,
    /// Permalink on the instance, when it has one.
    pub url: Option<String>,
    /// Attachments, in display order.
    pub media: Vec<PostMedia>,
    /// Whether the media should stay behind a click.
    pub sensitive: bool,
    /// Content warning, plain text.
    pub spoiler_text: String,
    /// Custom emoji used by the body.
    pub emojis: Vec<PostEmoji>,
}

/// Insert or refresh a batch of statuses.
///
/// Counts and content are replaced, never merged: they are a snapshot, and keeping
/// a stale maximum would make an old post look more popular than it is. The
/// `fetched_at`/`expires_at` pair is refreshed on conflict so a post that is still
/// being returned by the instance stays inside the retention window.
///
/// Returns the number of rows written. Non-retained visibilities are filtered out
/// in Rust before the statement, because a single row violating the table's
/// `CHECK` would abort the whole page.
///
/// # Errors
///
/// Propagates [`sqlx::Error`]. The caller must have ensured the `instance` and
/// `account` rows exist.
pub async fn upsert_many(pool: &PgPool, statuses: &[NewStatus]) -> Result<u64, sqlx::Error> {
    let retained: Vec<&NewStatus> = statuses
        .iter()
        .filter(|status| matches!(status.visibility.as_str(), "public" | "unlisted"))
        .collect();
    if retained.is_empty() {
        return Ok(0);
    }

    let hosts: Vec<String> = retained.iter().map(|s| s.instance_host.clone()).collect();
    let ids: Vec<i64> = retained.iter().map(|s| s.mastodon_id).collect();
    let accounts: Vec<i64> = retained.iter().map(|s| s.account_id).collect();
    let contents: Vec<String> = retained.iter().map(|s| s.content.clone()).collect();
    let visibilities: Vec<String> = retained.iter().map(|s| s.visibility.clone()).collect();
    let favourites: Vec<i32> = retained.iter().map(|s| s.favourites_count).collect();
    let reblogs: Vec<i32> = retained.iter().map(|s| s.reblogs_count).collect();
    let replies: Vec<i32> = retained.iter().map(|s| s.replies_count).collect();
    let languages: Vec<Option<String>> = retained.iter().map(|s| s.language.clone()).collect();
    let published: Vec<OffsetDateTime> = retained.iter().map(|s| s.published_at).collect();
    let urls: Vec<Option<String>> = retained.iter().map(|s| s.url.clone()).collect();
    let media: Vec<Value> = retained
        .iter()
        .map(|s| serde_json::to_value(&s.media).unwrap_or_else(|_| Value::Array(Vec::new())))
        .collect();
    let sensitives: Vec<bool> = retained.iter().map(|s| s.sensitive).collect();
    let spoilers: Vec<String> = retained.iter().map(|s| s.spoiler_text.clone()).collect();
    let emojis: Vec<Value> = retained
        .iter()
        .map(|s| serde_json::to_value(&s.emojis).unwrap_or_else(|_| Value::Array(Vec::new())))
        .collect();

    let result = sqlx::query(
        "INSERT INTO status (instance_host, mastodon_id, account_id, content, visibility, \
             favourites_count, reblogs_count, replies_count, language, created_at, url, \
             media, sensitive, spoiler_text, emojis) \
         SELECT host, mastodon_id, account_id, content, visibility, favourites_count, \
                reblogs_count, replies_count, language, created_at, url, media, sensitive, \
                spoiler_text, emojis \
           FROM UNNEST($1::text[], $2::bigint[], $3::bigint[], $4::text[], $5::text[], \
                       $6::int[], $7::int[], $8::int[], $9::text[], $10::timestamptz[], \
                       $11::text[], $12::jsonb[], $13::bool[], $14::text[], $15::jsonb[]) \
               AS page (host, mastodon_id, account_id, content, visibility, favourites_count, \
                        reblogs_count, replies_count, language, created_at, url, media, \
                        sensitive, spoiler_text, emojis) \
          WHERE visibility IN ('public', 'unlisted') \
         ON CONFLICT (instance_host, mastodon_id) DO UPDATE SET \
             content = EXCLUDED.content, \
             url = EXCLUDED.url, \
             media = EXCLUDED.media, \
             sensitive = EXCLUDED.sensitive, \
             spoiler_text = EXCLUDED.spoiler_text, \
             emojis = EXCLUDED.emojis, \
             favourites_count = EXCLUDED.favourites_count, \
             reblogs_count = EXCLUDED.reblogs_count, \
             replies_count = EXCLUDED.replies_count, \
             fetched_at = now(), \
             expires_at = now() + interval '14 days'",
    )
    .bind(&hosts)
    .bind(&ids)
    .bind(&accounts)
    .bind(&contents)
    .bind(&visibilities)
    .bind(&favourites)
    .bind(&reblogs)
    .bind(&replies)
    .bind(&languages)
    .bind(&published)
    .bind(&urls)
    .bind(&media)
    .bind(&sensitives)
    .bind(&spoilers)
    .bind(&emojis)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// The most recent statuses of one account, newest first.
///
/// Returns `(local id, content)`, which is all the post ranker needs: the text to
/// score and the row to point a recommendation at. Rows without content are
/// skipped because there is nothing to rank; the column is nullable even though
/// [`NewStatus`] always carries a string.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn account_posts(
    pool: &PgPool,
    account_id: i64,
    limit: i64,
) -> Result<Vec<(i64, String)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, String)>(
        "SELECT id, content \
           FROM status \
          WHERE account_id = $1 AND content IS NOT NULL \
          ORDER BY created_at DESC, id DESC \
          LIMIT $2",
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Record that the account's statuses were just fetched.
///
/// This lives beside the status write rather than in [`crate::accounts`] because
/// it is the second half of one ingestion step: the sweep that schedules
/// `refresh_account` reads `last_fetched_at`, so a status write that does not
/// stamp it would be fetched again on the next tick.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn mark_account_fetched(pool: &PgPool, account_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE account SET last_fetched_at = now(), updated_at = now() WHERE id = $1")
        .bind(account_id)
        .execute(pool)
        .await?;
    Ok(())
}
