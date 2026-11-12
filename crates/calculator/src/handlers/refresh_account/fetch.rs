//! The page walk behind `refresh_account`.
//!
//! [`run`] resolves the account's instance identity, spends one pooled token on
//! the host, walks the account's statuses and writes what is publicly readable.
//! [`map_status`] is the pure part — visibility, counts, language and timestamp —
//! and is what the unit tests exercise without a database.

use std::time::Duration;

use nodus_client_core::model::{
    LocalId, MediaAttachment, MediaKind, PageParams, Status, Visibility,
};
use nodus_client_core::{Error as InstanceError, Instance, InstanceApi};
use nodus_mastodon::{HttpInstance, ReqwestTransport};
use nodus_misskey::MisskeyInstance;
use nodus_shared::{PostEmoji, PostMedia};
use nodus_store::accounts;
use nodus_store::models::Account;
use nodus_store::statuses::{self, NewStatus};
use nodus_store::users;
use sqlx::PgPool;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::{info, warn};

use super::{JobError, RefreshConfig};

/// How long to wait before retrying when the host has no usable token.
///
/// The scheduler will enqueue the account again; five minutes is a compromise
/// between a token refill and re-reading a whole TTL sweep.
const NO_TOKEN_RETRY: Duration = Duration::from_secs(300);

/// Fetch and store one account's recent public statuses.
///
/// Returns the number of rows written. The account's `last_fetched_at` is stamped
/// even when the instance returned nothing, because the sweep that schedules this
/// job reads that column.
///
/// # Errors
///
/// [`JobError::Permanent`] when the account is gone or the instance refuses the
/// token; [`JobError::Retry`] for transport failures, rate limits, a database
/// error, and a pool with no token for the host.
pub(super) async fn run(
    pool: &PgPool,
    config: &RefreshConfig,
    account_id: i64,
) -> Result<u64, JobError> {
    let Some(account) = accounts::get(pool, account_id)
        .await
        .map_err(|error| database(&error))?
    else {
        return Err(JobError::permanent(format!(
            "account {account_id} no longer exists"
        )));
    };

    if let Some((user, origin)) = remote_acct(&account.acct) {
        match refresh_remote(pool, config, &account, user, origin).await {
            Ok(written) => {
                statuses::mark_account_fetched(pool, account_id)
                    .await
                    .map_err(|error| database(&error))?;
                info!(
                    account = account_id,
                    host = origin,
                    fetched = written,
                    "refresh_account finished from the author's instance"
                );
                return Ok(written);
            }
            Err(error) => warn!(
                account = account_id,
                host = origin,
                ?error,
                "origin refresh failed; trying the home instance"
            ),
        }
    }

    let sealed = pick_token(pool, &account.instance_host).await?;
    let api = transport(&account.instance_host, &sealed, config)?;
    let author = LocalId(account.mastodon_id.to_string());
    let (batch, avatar) = walk(&api, &author, &account.instance_host, account.id, config).await?;

    let written = statuses::upsert_many(pool, &batch)
        .await
        .map_err(|error| database(&error))?;
    if let Some(avatar) = avatar {
        accounts::set_avatar(pool, account_id, &avatar)
            .await
            .map_err(|error| database(&error))?;
    }
    statuses::mark_account_fetched(pool, account_id)
        .await
        .map_err(|error| database(&error))?;

    info!(
        account = account_id,
        host = %account.instance_host,
        fetched = batch.len(),
        written,
        "refresh_account finished"
    );
    Ok(written)
}

/// The `user@host` an account actually lives on, when it is not local.
fn remote_acct(acct: &str) -> Option<(&str, &str)> {
    acct.split_once('@')
        .filter(|(user, host)| !user.is_empty() && !host.is_empty())
}

/// Read the canonical posts from the author's own instance, without a token.
///
/// Mastodon exposes public statuses unauthenticated. That makes the recommender
/// independent of the home instance's cache and of a pooled token for every host.
async fn refresh_remote(
    pool: &PgPool,
    config: &RefreshConfig,
    account: &Account,
    user: &str,
    origin: &str,
) -> Result<u64, JobError> {
    users::ensure_instance(pool, origin)
        .await
        .map_err(|error| database(&error))?;
    let api = public_transport(origin, config)?;
    let (remote, batch, avatar) = match api.account_lookup(user).await {
        Ok(remote) => {
            let author = LocalId(remote.id.clone());
            let (batch, avatar) = walk(&api, &author, origin, account.id, config).await?;
            (remote, batch, avatar)
        }
        Err(InstanceError::Status { status: 404, .. }) => {
            let api = misskey_transport(origin, config)?;
            let remote = api.account_lookup(user).await.map_err(classify)?;
            let author = LocalId(remote.id.clone());
            let (batch, avatar) = walk(&api, &author, origin, account.id, config).await?;
            (remote, batch, avatar)
        }
        Err(error) => return Err(classify(error)),
    };
    accounts::set_remote_id(pool, account.id, &remote.id)
        .await
        .map_err(|error| database(&error))?;

    let mut fresh = Vec::with_capacity(batch.len());
    for row in &batch {
        if !statuses::update_by_url(pool, row)
            .await
            .map_err(|error| database(&error))?
        {
            fresh.push(row.clone());
        }
    }
    statuses::upsert_many(pool, &fresh)
        .await
        .map_err(|error| database(&error))?;
    if let Some(avatar) = avatar {
        accounts::set_avatar(pool, account.id, &avatar)
            .await
            .map_err(|error| database(&error))?;
    }
    Ok(u64::try_from(batch.len()).unwrap_or(u64::MAX))
}

/// Chooses the token that may read this host's public statuses.
///
/// The pool is shared, so a 401 on one token must not stop the run; the caller
/// only needs *a* token whose owner consented to background reads.
async fn pick_token(pool: &PgPool, host: &str) -> Result<Vec<u8>, JobError> {
    let Some(pooled) = users::pick_token(pool, host, &[])
        .await
        .map_err(|error| database(&error))?
    else {
        return Err(JobError::retry(
            NO_TOKEN_RETRY,
            format!("no token for host {host}; the refresh waits for one"),
        ));
    };
    Ok(pooled.access_token_enc)
}

/// Opens a sealed token and builds the transport for one instance.
pub(crate) fn public_transport(
    host: &str,
    config: &RefreshConfig,
) -> Result<HttpInstance, JobError> {
    let authority = format!("{}://{host}", config.scheme);
    let instance = Instance::parse(&authority).map_err(|error| {
        JobError::permanent(format!("`{host}` is not a usable instance: {error}"))
    })?;
    Ok(HttpInstance::new(&instance, String::new()))
}

/// Build the Misskey client for an origin that is not Mastodon.
pub(crate) fn misskey_transport(
    host: &str,
    config: &RefreshConfig,
) -> Result<MisskeyInstance<ReqwestTransport>, JobError> {
    let authority = format!("{}://{host}", config.scheme);
    let instance = Instance::parse(&authority).map_err(|error| {
        JobError::permanent(format!("`{host}` is not a usable instance: {error}"))
    })?;
    Ok(MisskeyInstance::with_transport(
        &instance,
        String::new(),
        ReqwestTransport::new(),
    ))
}

pub(crate) fn transport(
    host: &str,
    sealed: &[u8],
    config: &RefreshConfig,
) -> Result<HttpInstance, JobError> {
    let secret = config.sealer.open(sealed).map_err(|error| {
        // One corrupt row must not make the account permanently unrefreshable;
        // the pool rotates, and the next attempt may pick a healthy token.
        JobError::retry(
            NO_TOKEN_RETRY,
            format!("a pooled token for {host} cannot be decrypted: {error}"),
        )
    })?;
    let authority = format!("{}://{host}", config.scheme);
    let instance = Instance::parse(&authority).map_err(|error| {
        JobError::permanent(format!("`{host}` is not a usable instance: {error}"))
    })?;
    Ok(HttpInstance::new(&instance, secret))
}

/// Walks at most `max_pages` pages, sleeping between requests.
///
/// The sleep is between pages rather than before the first one: a fresh job that
/// has not spent a request should not wait for a budget it has not touched.
async fn walk<I>(
    api: &I,
    author: &LocalId,
    instance_host: &str,
    account_id: i64,
    config: &RefreshConfig,
) -> Result<(Vec<NewStatus>, Option<String>), JobError>
where
    I: InstanceApi<Error = InstanceError>,
{
    let mut params = PageParams::first(config.page_size);
    let mut batch = Vec::new();
    let mut avatar = None;

    for page in 0..config.max_pages {
        if page > 0 {
            tokio::time::sleep(config.request_interval).await;
        }
        let result = api
            .account_statuses(author, params)
            .await
            .map_err(classify)?;
        if result.items.is_empty() {
            break;
        }
        if avatar.is_none() {
            avatar = result
                .items
                .iter()
                .find(|status| !status.account.avatar.trim().is_empty())
                .map(|status| status.account.avatar.clone());
        }
        batch.extend(
            result
                .items
                .iter()
                .filter_map(|status| map_status(instance_host, account_id, status)),
        );
        let Some(cursor) = result.next_max_id else {
            break;
        };
        params = PageParams::after(cursor, config.page_size);
    }

    Ok((batch, avatar))
}

/// Keep one status if it is publicly readable, as a row the store can write.
///
/// A status whose id or timestamp does not parse is dropped rather than failing
/// the page: one malformed item must not cost the whole sweep. Counts saturate at
/// `i32::MAX` because the table stores `integer`; a broken counter is visibly
/// wrong instead of silently zero.
///
/// The SDK's [`Status`] does not model Mastodon's `language` field yet, so the
/// nullable column is left empty here; adding it to the model is a one-line change
/// once the transports parse it.
pub(crate) fn map_status(
    instance_host: &str,
    account_id: i64,
    status: &Status,
) -> Option<NewStatus> {
    if !matches!(status.visibility, Visibility::Public | Visibility::Unlisted) {
        return None;
    }
    let remote_id = status.id.clone();
    let mastodon_id = remote_id
        .parse::<i64>()
        .unwrap_or_else(|_| stable_id(instance_host, &remote_id));
    let published_at = OffsetDateTime::parse(&status.created_at, &Rfc3339).ok()?;

    Some(NewStatus {
        instance_host: instance_host.to_owned(),
        mastodon_id,
        remote_id,
        account_id,
        content: status.content.clone(),
        visibility: status.visibility.as_str().to_owned(),
        favourites_count: count(status.favourites_count),
        reblogs_count: count(status.reblogs_count),
        replies_count: count(status.replies_count),
        language: None,
        published_at,
        url: status.url.clone(),
        media: status.media_attachments.iter().map(post_media).collect(),
        sensitive: status.sensitive,
        spoiler_text: status.spoiler_text.clone(),
        emojis: status.emojis.iter().map(post_emoji).collect(),
    })
}

/// A stable signed id for an origin whose ids are not integers.
///
/// FNV-1a over `host|id`; the top bit is cleared so the value fits `bigint`.
fn stable_id(host: &str, id: &str) -> i64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in host.bytes().chain(std::iter::once(b'|')).chain(id.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    i64::try_from(hash & 0x7fff_ffff_ffff_ffff).unwrap_or(i64::MAX)
}

/// The wire shape keeps Mastodon's `type` spelling between the two structs.
fn post_media(media: &MediaAttachment) -> PostMedia {
    PostMedia {
        id: media.id.clone(),
        kind: match media.kind {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Gifv => "gifv",
            MediaKind::Audio => "audio",
            MediaKind::Unknown => "unknown",
        }
        .to_owned(),
        url: media.url.clone(),
        preview_url: media.preview_url.clone(),
        description: media.description.clone(),
        blurhash: media.blurhash.clone(),
        meta_width: media.meta_width,
        meta_height: media.meta_height,
    }
}

fn post_emoji(emoji: &nodus_client_core::model::CustomEmoji) -> PostEmoji {
    PostEmoji {
        shortcode: emoji.shortcode.clone(),
        url: emoji.url.clone(),
        static_url: emoji.static_url.clone(),
    }
}

/// A `u64` counter narrowed to the table's `integer` column, saturating.
fn count(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Maps the instance's errors onto the queue's retry vocabulary.
///
/// 401/403/404 are permanent: the token is dead or the author is gone, and a retry
/// would spend the host's budget on the same answer. A 429 carries the instance's
/// own delay, which is also how `SYNC_MAX_RPS_PER_HOST` pressure surfaces — the
/// queue reschedules instead of the worker sleeping through a full budget.
fn classify(error: InstanceError) -> JobError {
    match error {
        InstanceError::RateLimited { retry_after_s } => JobError::retry(
            Duration::from_secs(u64::from(retry_after_s.max(1))),
            format!("instance rate limit: retry after {retry_after_s}s"),
        ),
        InstanceError::Unauthorized => {
            JobError::permanent("401 from the instance: token revoked or scope removed")
        }
        InstanceError::Status {
            status: 403,
            message,
        } => JobError::permanent(format!(
            "403 from the instance: the account is gone or limited: {message}"
        )),
        InstanceError::Status {
            status: 404,
            message,
        } => JobError::permanent(format!(
            "404 from the instance: the account no longer exists: {message}"
        )),
        InstanceError::Status { status, message } => JobError::retry(
            Duration::from_secs(300),
            format!("instance answered {status}: {message}"),
        ),
        InstanceError::Transport(message) => {
            JobError::retry(Duration::from_secs(60), format!("transport: {message}"))
        }
        InstanceError::Decode(message) => JobError::retry(
            Duration::from_secs(300),
            format!("unexpected response shape: {message}"),
        ),
    }
}

/// A failed write is retried: the fetch succeeded, so throwing the page away
/// would spend the host's budget on it again later.
fn database(error: &sqlx::Error) -> JobError {
    JobError::retry(Duration::from_secs(30), format!("database: {error}"))
}
