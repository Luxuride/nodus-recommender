//! The data-subject access export behind `GET /api/v1/me/export`.
//!
//! One document per user, shaped for a person to read. The rows below are already the
//! final shape, so the handler serializes what the store returns and the export cannot
//! drift from what is stored by being re-assembled somewhere above this layer.
//!
//! Two properties are deliberate:
//!
//! * **No credentials.** `app_user.access_token_enc` and `app_user.token_fingerprint`
//!   are a credential and a verifier for it, not data we are obliged to hand back, and
//!   an export travels through a browser download. The queries never select them, and
//!   the `serialized_export_carries_no_credentials` test fails if a field ever
//!   reintroduces them.
//! * **Everything, deterministically.** Each section is its own ordered query, no row
//!   limits except the event cap — an export that silently truncated the follow graph
//!   would be worse than no export.
//!
//! Sections are separate statements, not one transaction: the export is a consistent
//! enough snapshot, and holding a read transaction open while a 10 000-row event page
//! goes out over a slow connection would block vacuum.

use serde::Serialize;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// Rows of `interaction_event` a single export returns.
///
/// The cap exists so one pathological telemetry history cannot turn the endpoint into a
/// long transfer; the newest rows are kept, because they are the ones a user recognises.
const EVENT_EXPORT_LIMIT: i64 = 10_000;

/// Everything we hold about one user.
#[derive(Debug, Serialize)]
pub struct UserExport {
    /// When this document was assembled, so an old file cannot be mistaken for current.
    pub generated_at: OffsetDateTime,
    pub user: ExportUser,
    pub follows: Vec<ExportFollow>,
    pub engagements: Vec<ExportEngagement>,
    pub events: Vec<ExportEvent>,
    pub recommendations: Vec<ExportRecommendation>,
}

/// The account itself.
///
/// Never contains `access_token_enc` or `token_fingerprint`: the stored token is a
/// credential, and its fingerprint is a verifier for that credential. Both are omitted
/// from the row and from the JSON.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ExportUser {
    pub id: Uuid,
    /// `citext` in the database, `::text` in the query.
    pub instance_host: String,
    /// The account id on that instance (`app_user.mastodon_account_id`).
    pub mastodon_id: i64,
    pub acct: String,
    pub sync_enabled: bool,
    pub telemetry_opt_in: bool,
    pub created_at: OffsetDateTime,
    /// Our last contact with the account (`app_user.last_seen_at`); `None` means we have
    /// never read it back, which is what "never synced" looks like to a user.
    pub last_synced_at: Option<OffsetDateTime>,
}

/// One account the user follows, as we observed it.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ExportFollow {
    pub account_acct: String,
    /// The followed account's id on its own instance.
    pub account_id: i64,
    /// First time we observed the edge (`follow_edge.first_seen_at`).
    pub created_at: OffsetDateTime,
}

/// One aggregated engagement edge: how often, and how recently, the user acted on an
/// account's posts.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ExportEngagement {
    pub account_acct: String,
    /// `favourite`, `reblog` or `reply`.
    pub kind: String,
    pub count: i32,
    pub last_at: OffsetDateTime,
}

/// One served item the user interacted with.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ExportEvent {
    /// `recommendation_item.id`; the item itself may already have expired.
    pub item_id: i64,
    /// `impression`, `click`, `follow`, `dismiss`, `not_interested`, `boost` or `reply`.
    pub kind: String,
    /// Our receive time; client clocks are not trusted.
    pub server_ts: OffsetDateTime,
}

/// One account we recommended to the user.
///
/// Post recommendations are not listed: they point at a status that expires after 14
/// days, so an item whose post is gone has nothing left to show.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ExportRecommendation {
    pub account_acct: String,
    pub score: f32,
    /// Followers at export time, not at serving time; it is context, not history.
    pub followers_count: i64,
    /// When we served the item (`recommendation_item.served_at`).
    pub generated_at: OffsetDateTime,
}

/// The user row, with `last_seen_at` presented as `last_synced_at`.
///
/// Soft-deleted users are still exported: the row stays theirs until the purge job
/// removes it, and "give me my data" is exactly the request someone makes on the way
/// out.
const SELECT_USER: &str = "SELECT id, instance_host::text AS instance_host, \
     mastodon_account_id AS mastodon_id, acct::text AS acct, sync_enabled, \
     telemetry_opt_in, created_at, last_seen_at AS last_synced_at \
     FROM app_user WHERE id = $1";

/// The user's account row is the one matching their own `(instance, account id)`: the
/// follow and engagement edges are keyed on `account.id`, not on the user id.
const SELECT_FOLLOWS: &str = "SELECT followee.acct::text AS account_acct, \
     followee.mastodon_id AS account_id, edge.first_seen_at AS created_at \
     FROM app_user AS u \
     JOIN account AS me \
         ON me.instance_host = u.instance_host AND me.mastodon_id = u.mastodon_account_id \
     JOIN follow_edge AS edge ON edge.follower_id = me.id \
     JOIN account AS followee ON followee.id = edge.followee_id \
     WHERE u.id = $1 \
     ORDER BY created_at";

const SELECT_ENGAGEMENTS: &str = "SELECT target.acct::text AS account_acct, edge.kind, \
     edge.count, edge.last_seen_at AS last_at \
     FROM app_user AS u \
     JOIN account AS me \
         ON me.instance_host = u.instance_host AND me.mastodon_id = u.mastodon_account_id \
     JOIN engagement_edge AS edge ON edge.actor_id = me.id \
     JOIN account AS target ON target.id = edge.target_id \
     WHERE u.id = $1 \
     ORDER BY last_at";

/// Newest first, with `id` as the tiebreaker so the 10 000-row cap is a stable page
/// rather than an arbitrary one when several events share a timestamp.
const SELECT_EVENTS: &str = "SELECT item_id, kind, server_ts FROM interaction_event \
     WHERE user_id = $1 \
     ORDER BY server_ts DESC, id DESC \
     LIMIT $2";

/// Account recommendations only: post items carry a `status_id` and no `account_id`, so
/// the join drops them rather than exporting a recommendation with nothing to name it.
const SELECT_RECOMMENDATIONS: &str = "SELECT a.acct::text AS account_acct, item.score, \
     a.followers_count::bigint AS followers_count, item.served_at AS generated_at \
     FROM recommendation_item AS item \
     JOIN account AS a ON a.id = item.account_id \
     WHERE item.user_id = $1 \
     ORDER BY generated_at";

/// Assemble everything we hold about `user_id`.
///
/// Returns `None` when there is no such user, which the handler turns into a 404 — the
/// distinction between "no data" and "not our user" has to survive this layer.
///
/// # Errors
///
/// Propagates [`sqlx::Error`] from the first failing section. A failure leaves no
/// partial document: the whole export is built in memory before it is returned.
pub async fn export_user(pool: &PgPool, user_id: Uuid) -> Result<Option<UserExport>, sqlx::Error> {
    let Some(user) = sqlx::query_as::<_, ExportUser>(SELECT_USER)
        .bind(user_id)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };

    let follows = sqlx::query_as::<_, ExportFollow>(SELECT_FOLLOWS)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    let engagements = sqlx::query_as::<_, ExportEngagement>(SELECT_ENGAGEMENTS)
        .bind(user_id)
        .fetch_all(pool)
        .await?;
    let events = sqlx::query_as::<_, ExportEvent>(SELECT_EVENTS)
        .bind(user_id)
        .bind(EVENT_EXPORT_LIMIT)
        .fetch_all(pool)
        .await?;
    let recommendations = sqlx::query_as::<_, ExportRecommendation>(SELECT_RECOMMENDATIONS)
        .bind(user_id)
        .fetch_all(pool)
        .await?;

    Ok(Some(UserExport {
        generated_at: OffsetDateTime::now_utc(),
        user,
        follows,
        engagements,
        events,
        recommendations,
    }))
}

#[cfg(test)]
mod tests {
    use super::{ExportEngagement, ExportEvent, ExportFollow, ExportRecommendation};
    use super::{ExportUser, UserExport};
    use time::OffsetDateTime;
    use uuid::Uuid;

    fn sample_export() -> UserExport {
        let at = OffsetDateTime::UNIX_EPOCH;
        UserExport {
            generated_at: at,
            user: ExportUser {
                id: Uuid::nil(),
                instance_host: "example.social".to_owned(),
                mastodon_id: 1,
                acct: "alice@example.social".to_owned(),
                sync_enabled: true,
                telemetry_opt_in: false,
                created_at: at,
                last_synced_at: None,
            },
            follows: vec![ExportFollow {
                account_acct: "bob@example.social".to_owned(),
                account_id: 2,
                created_at: at,
            }],
            engagements: vec![ExportEngagement {
                account_acct: "bob@example.social".to_owned(),
                kind: "favourite".to_owned(),
                count: 3,
                last_at: at,
            }],
            events: vec![ExportEvent {
                item_id: 7,
                kind: "click".to_owned(),
                server_ts: at,
            }],
            recommendations: vec![ExportRecommendation {
                account_acct: "bob@example.social".to_owned(),
                score: 0.5,
                followers_count: 42,
                generated_at: at,
            }],
        }
    }

    #[test]
    fn serialized_export_carries_no_credentials() {
        let json = serde_json::to_string(&sample_export()).expect("export serializes");
        assert!(
            !json.contains("token"),
            "the export must not carry the token: {json}"
        );
        assert!(
            !json.contains("enc"),
            "the export must not carry the encrypted token: {json}"
        );
    }

    #[test]
    fn every_section_is_present_in_the_json() {
        let json = serde_json::to_value(sample_export()).expect("export serializes");
        for section in [
            "generated_at",
            "user",
            "follows",
            "engagements",
            "events",
            "recommendations",
        ] {
            assert!(json.get(section).is_some(), "missing section {section}");
        }
    }
}
