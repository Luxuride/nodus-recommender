//! Lifting a rejection: undo is a delete, not a counter-event.
//!
//! The feed reads a rejection as `(kind, recorded_at)` and decays it (D26), so an
//! absent rejection already means "no evidence" to every query that looks at one.
//! Recording an opposite kind instead (`undismiss`) would need its own half-life and
//! its own line in each of those queries — including the candidate and popularity
//! ones — and the two spellings would be free to disagree. Deleting the event keeps
//! the rule in one place: a rejection exists or it does not.
//!
//! [`lift`] therefore removes the row, and the next [`crate::feed::candidates`] call
//! for that item's account sees no rejection. There is no cache to invalidate: the
//! candidate query reads `interaction_event` inside the request that ranks.

use sqlx::PgPool;
use uuid::Uuid;

/// The rejection a [`lift`] call removed.
///
/// Returned rather than a bare `bool` so the caller can log *which* decision was
/// undone: a `dismiss` and a `not_interested` are different statements, and an
/// operator reading a trace should not have to guess.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Lifted {
    /// The served item the removed event named.
    pub item_id: i64,
    /// `dismiss` or `not_interested`.
    pub kind: String,
}

/// Remove the rejection in force for one item, returning what was removed.
///
/// Only the most recent `dismiss`/`not_interested` is deleted: the user is undoing
/// the decision they just made on this card, and an earlier rejection of the same
/// item was already superseded by the newer one. Deleting it changes exactly what
/// the user asked to change.
///
/// `Ok(None)` means the item carried no rejection, which is a legitimate answer
/// rather than an error: the client asked to undo something that was not there.
///
/// # Errors
///
/// Propagates [`sqlx::Error`] when the delete cannot be run.
pub async fn lift(
    pool: &PgPool,
    user_id: Uuid,
    item_id: i64,
) -> Result<Option<Lifted>, sqlx::Error> {
    sqlx::query_as::<_, Lifted>(
        "DELETE FROM interaction_event \
         WHERE id = ( \
             SELECT id FROM interaction_event \
             WHERE user_id = $1 AND item_id = $2 \
                 AND kind IN ('dismiss', 'not_interested') \
             ORDER BY server_ts DESC, id DESC \
             LIMIT 1 \
         ) \
         RETURNING item_id, kind",
    )
    .bind(user_id)
    .bind(item_id)
    .fetch_optional(pool)
    .await
}
