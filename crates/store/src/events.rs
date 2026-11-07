//! Interaction telemetry, and the one place a follow changes the cache.
//!
//! Two properties make the write path safe to retry and safe to be wrong about:
//!
//! * **Idempotency is derived, not assigned.** The client never invents a key; we hash
//!   the observation itself ([`idempotency_key`]), so a batch re-sent after a timeout
//!   cannot double-count an impression.
//! * **Foreign items are dropped, not rejected.** An event naming an item that was
//!   never served to this user is either a client bug or an attempt to poison the
//!   ranking data. Neither deserves an error the caller can distinguish from a
//!   successful write, so [`record`] silently ignores those rows.
//!
//! [`mark_followed`] is the counterweight: a follow is the strongest positive signal
//! the engine gets, and the affinity and follow graph it was derived from are stale the
//! moment it happens, so the cached affinity row is dropped and a P0 sync is queued.

use crate::jobs::{self, NewJob};
use base64::Engine as _;
use nodus_shared::{EventKind, InteractionEvent};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// How far a client clock may be from ours before the timestamp is clamped.
///
/// Retention and frequency capping are measured on `server_ts`; `client_ts` only orders
/// events within a session, so a wildly wrong clock is clamped rather than rejected.
const CLOCK_WINDOW: time::Duration = time::Duration::seconds(10 * 60);

/// The database spelling of an [`EventKind`].
///
/// The wire form is `snake_case` and the `interaction_event.kind` check constraint uses
/// the same spelling. Keeping the mapping here means the value hashed into an
/// idempotency key and the value inserted in the same statement cannot drift apart.
#[must_use]
pub const fn kind_text(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Impression => "impression",
        EventKind::Click => "click",
        EventKind::Follow => "follow",
        EventKind::Dismiss => "dismiss",
        EventKind::NotInterested => "not_interested",
        EventKind::Boost => "boost",
        EventKind::Reply => "reply",
    }
}

/// A stable key for one observation, so a retried batch cannot double-count.
///
/// The hash covers `item_id`, `kind` and the client timestamp **to the second** — the
/// three things that say *which observation this is*. It deliberately excludes
/// `dwell_ms` and the rest of the event: a client that re-sends a batch after a
/// timeout may have measured the dwell again (or not at all), but the user still saw
/// the same item once, and an impression that becomes two impressions because the
/// retry was 30 ms later would corrupt every frequency-capping and exposure count
/// downstream. Second granularity is the same trade-off at a smaller scale: two
/// clicks on one item inside one second collapse into one, which is below the
/// resolution any ranking feature actually uses.
///
/// The result is a `bytea` in the database, where the unique constraint
/// `(user_id, idempotency_key)` makes the drop happen.
#[must_use]
pub fn idempotency_key(item_id: i64, kind: &str, client_ts: OffsetDateTime) -> [u8; 32] {
    let unix_seconds = client_ts.unix_timestamp();
    let mut hasher = Sha256::new();
    hasher.update(format!("{item_id}:{kind}:{unix_seconds}").as_bytes());
    hasher.finalize().into()
}

/// Record a batch of events for one user, returning how many rows were inserted.
///
/// One statement covers the whole batch: the per-event rows are unnested into a
/// `SELECT` that is joined against `recommendation_item` for the caller. That join is
/// the ownership check — an item served to somebody else never reaches the insert, so
/// it is silently dropped rather than reported. `ON CONFLICT (user_id,
/// idempotency_key) DO NOTHING` turns a retried batch into a no-op, and the returned
/// count is what actually landed (so a retry over the same data returns `0`).
///
/// `client_ts` is clamped to [`CLOCK_WINDOW`] around our clock after the idempotency
/// key has been taken from the unclamped value: clamping before hashing would give the
/// same event a different key on every retry.
///
/// `feed_id` is the snapshot the client is reporting against. The event table has no
/// feed column, so it is carried into the trace line to correlate a batch with the feed
/// response that produced it; ownership is still decided per item.
///
/// # Errors
///
/// Propagates [`sqlx::Error`]. A `client_ts` that is not RFC 3339 surfaces as
/// [`sqlx::Error::Decode`]; the HTTP layer validates it first, so that is a bug rather
/// than a client error by the time it gets here.
pub async fn record(
    pool: &PgPool,
    user_id: Uuid,
    feed_id: Uuid,
    events: &[(InteractionEvent, [u8; 32])],
) -> Result<u64, sqlx::Error> {
    if events.is_empty() {
        return Ok(0);
    }

    let now = OffsetDateTime::now_utc();
    let mut item_ids: Vec<i64> = Vec::with_capacity(events.len());
    let mut kind_texts: Vec<String> = Vec::with_capacity(events.len());
    let mut timestamps: Vec<OffsetDateTime> = Vec::with_capacity(events.len());
    let mut dwells: Vec<Option<i32>> = Vec::with_capacity(events.len());
    let mut encoded_keys: Vec<String> = Vec::with_capacity(events.len());

    for (event, key) in events {
        let client_ts = OffsetDateTime::parse(&event.client_ts, &Rfc3339)
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        item_ids.push(event.item_id);
        kind_texts.push(kind_text(event.kind).to_owned());
        timestamps.push(clamp_to_clock_window(client_ts, now));
        dwells.push(dwell_for(event.kind, event.dwell_ms));
        encoded_keys.push(base64::engine::general_purpose::STANDARD.encode(key));
    }

    let result = sqlx::query(
        "INSERT INTO interaction_event \
             (user_id, item_id, kind, client_ts, dwell_ms, idempotency_key) \
         SELECT $1, batch.item_id, batch.kind, batch.client_ts, batch.dwell_ms, \
             decode(batch.key, 'base64') \
         FROM UNNEST($2::bigint[], $3::text[], $4::timestamptz[], $5::int[], $6::text[]) \
             AS batch (item_id, kind, client_ts, dwell_ms, key) \
         WHERE EXISTS ( \
             SELECT 1 FROM recommendation_item ri \
             WHERE ri.id = batch.item_id AND ri.user_id = $1 \
         ) \
         ON CONFLICT (user_id, idempotency_key) DO NOTHING",
    )
    .bind(user_id)
    .bind(&item_ids)
    .bind(&kind_texts)
    .bind(&timestamps)
    .bind(&dwells)
    .bind(&encoded_keys)
    .execute(pool)
    .await?;

    let inserted = result.rows_affected();
    tracing::debug!(
        %feed_id,
        user_id = %user_id,
        submitted = events.len(),
        inserted,
        "interaction events recorded"
    );
    Ok(inserted)
}

/// Whether a served item belongs to this user.
///
/// Feedback has to answer with `404` for an item that is not the caller's, which the
/// silent drop in [`record`] cannot express. This is the lookup for that one case.
///
/// # Errors
///
/// Propagates [`sqlx::Error`].
pub async fn item_owned(pool: &PgPool, user_id: Uuid, item_id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS ( \
             SELECT 1 FROM recommendation_item WHERE id = $1 AND user_id = $2 \
         )",
    )
    .bind(item_id)
    .bind(user_id)
    .fetch_one(pool)
    .await
}

/// Invalidate what a follow makes wrong: the cached affinity, then the graph itself.
///
/// The affinity row is deleted because it is a *prediction* that this account is worth
/// recommending; after a follow that prediction is stale in the strongest possible
/// direction. The P0 `sync_user` job is queued because the follow graph just changed,
/// and `user_similarity`/`account_affinity` can only be recomputed from a fresh one.
/// Enqueueing reuses the job queue's idempotency, so a user who follows ten accounts
/// from one page still gets one outstanding sync at P0.
///
/// # Errors
///
/// Propagates [`sqlx::Error`]. The two writes are not in one transaction on purpose:
/// the delete is idempotent and the enqueue is deduplicated, so a partial run is fixed
/// by the retry rather than needing a rollback.
pub async fn mark_followed(
    pool: &PgPool,
    user_id: Uuid,
    account_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM account_affinity WHERE user_id = $1 AND account_id = $2")
        .bind(user_id)
        .bind(account_id)
        .execute(pool)
        .await?;
    let _ = jobs::enqueue(pool, &NewJob::sync_user(&user_id)).await?;
    Ok(())
}

/// Pull a client timestamp into [`CLOCK_WINDOW`] of our clock.
fn clamp_to_clock_window(client_ts: OffsetDateTime, now: OffsetDateTime) -> OffsetDateTime {
    let earliest = now - CLOCK_WINDOW;
    let latest = now + CLOCK_WINDOW;
    if client_ts < earliest {
        earliest
    } else if client_ts > latest {
        latest
    } else {
        client_ts
    }
}

/// The dwell a client reported, or `None` when we do not want it.
///
/// Only an impression carries attention: dwell is how long a *rendered* item held
/// the reader, so it belongs to the event that says the item was rendered. A follow
/// or a dismiss is an outcome, not a measurement, and a client reporting dwell on
/// one is reporting something we did not ask for.
///
/// The cap is the documented 120-second bound from `nodus_core::evidence`: an
/// overnight tab is not attention. The column is `integer`, so clamping also keeps a
/// nonsense value from having to be handled as an overflow.
fn dwell_for(kind: EventKind, dwell_ms: Option<u32>) -> Option<i32> {
    match (kind, dwell_ms) {
        (EventKind::Impression, Some(dwell)) => {
            i32::try_from(dwell.min(nodus_core::evidence::DWELL_CAP_MS)).ok()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(unix_seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix_seconds).unwrap()
    }

    #[test]
    fn only_an_impression_carries_measured_attention() {
        assert_eq!(dwell_for(EventKind::Impression, Some(4_000)), Some(4_000));
        assert_eq!(dwell_for(EventKind::Impression, None), None);
        // An outcome is not a measurement, even when the client sends one.
        assert_eq!(dwell_for(EventKind::Follow, Some(4_000)), None);
        assert_eq!(dwell_for(EventKind::Click, Some(4_000)), None);
    }

    #[test]
    fn an_overnight_tab_is_clamped_to_the_cap() {
        let capped = dwell_for(EventKind::Impression, Some(u32::MAX));
        assert_eq!(
            capped,
            Some(i32::try_from(nodus_core::evidence::DWELL_CAP_MS).unwrap())
        );
    }

    #[test]
    fn idempotency_key_is_stable_across_calls() {
        let first = idempotency_key(7, "impression", at(1_760_000_000));
        let second = idempotency_key(7, "impression", at(1_760_000_000));
        assert_eq!(first, second);
    }

    #[test]
    fn different_kinds_get_different_keys() {
        let impression = idempotency_key(7, "impression", at(1_760_000_000));
        let click = idempotency_key(7, "click", at(1_760_000_000));
        assert_ne!(impression, click);
    }

    #[test]
    fn different_items_get_different_keys() {
        let first = idempotency_key(7, "impression", at(1_760_000_000));
        let second = idempotency_key(8, "impression", at(1_760_000_000));
        assert_ne!(first, second);
    }

    #[test]
    fn dwell_does_not_change_the_key() {
        // Two reports of the same impression, measured differently. The key is taken
        // from item, kind and second, so a re-measured dwell is still the same event.
        let brief = InteractionEvent {
            item_id: 7,
            kind: EventKind::Impression,
            client_ts: "2025-10-09T08:53:20Z".to_owned(),
            dwell_ms: Some(900),
        };
        let long = InteractionEvent {
            dwell_ms: Some(4_000),
            ..brief.clone()
        };
        let brief_ts = OffsetDateTime::parse(&brief.client_ts, &Rfc3339).unwrap();
        let long_ts = OffsetDateTime::parse(&long.client_ts, &Rfc3339).unwrap();
        assert_eq!(
            idempotency_key(brief.item_id, kind_text(brief.kind), brief_ts),
            idempotency_key(long.item_id, kind_text(long.kind), long_ts)
        );
    }

    #[test]
    fn a_different_second_changes_the_key() {
        let first = idempotency_key(7, "impression", at(1_760_000_000));
        let second = idempotency_key(7, "impression", at(1_760_000_001));
        assert_ne!(first, second);
    }
}
