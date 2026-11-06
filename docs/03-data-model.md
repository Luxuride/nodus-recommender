# 03 — Data model

Status: draft · Last revised: 2026-11-06 (revision 3: no sessions, browser-held tokens)

The database is the service's memory and its largest recurring cost. Every table
below states its **retention** and **budget**, because "store everything" is not
an option: at 1M tracked accounts the follow graph alone can reach tens of
millions of edges.

Schema changes are **only** made through `sqlx migrate` files in `migrations/`.
No init scripts, no hand-run DDL, no schema writes from application code.

## Naming and type conventions

- snake_case tables and columns, plural table names.
- Internal identity: `bigint generated always as identity` primary keys, except
  `app_user` which uses `uuid` (it is a public-ish identifier and appears in URLs).
- Mastodon ids are `bigint` (they are i64 today); they are only meaningful
  together with `instance_host`.
- Handles use `citext` so `Alice@Example.social` and `alice@example.social` are
  one row. `acct` is `username@host` (no leading `@`).
- Timestamps are `timestamptz`; all code uses UTC.
- Nullability is intentional: `NULL` means "we have not observed it", `false`
  means "observed as false".

## Tables

### `instance`
Metadata about a fediverse host we talk to.

| Column | Type | Notes |
| --- | --- | --- |
| `host` | citext PK | `example.social` |
| `software` | text NULL | from `/api/v2/instance` (`mastodon`, `glitch-soc`, …) |
| `version` | text NULL | |
| `rate_limit` | integer NULL | discovered `X-RateLimit-Limit`, default 300 |
| `blocked` | boolean NOT NULL default false | instance we refuse to crawl (policy) |
| `last_ok_at` / `last_error_at` | timestamptz NULL | crawl health |
| `last_error` | text NULL | truncated |

### `app_user`
Our users — the only accounts we may hold tokens for.

The token arrives from the browser (see [08-sdk.md](08-sdk.md) and
[09-privacy.md](09-privacy.md)); we never run an OAuth redirect of our own and
we register no application per instance. `access_token_enc` is populated only
when the user grants background sync.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | uuid PK | |
| `instance_host` / `mastodon_id` | citext / bigint | the user's own account, unique pair |
| `account_id` | bigint → `account` | denormalized link to their tracked account |
| `access_token_enc` | bytea NULL | AEAD-encrypted, key from `TOKEN_ENC_KEY`; NULL when sync is off |
| `token_scopes` | text[] | granted scopes; `write:follows` present only if opted in |
| `token_fingerprint` | bytea | `sha256(token)` prefix, for the verify cache — not reversible |
| `sync_enabled` | boolean NOT NULL default false | server-side sync consent |
| `telemetry_opt_in` | boolean NOT NULL default false | gates `interaction_event` writes |
| `telemetry_opt_in_at` | timestamptz NULL | |
| `created_at`, `last_seen_at` | timestamptz | |
| `deleted_at` | timestamptz NULL | soft delete, hard purge 30 days later |

### `account`
Every fediverse account we know anything about (≈ every followee we ingest).

| Column | Type | Notes |
| --- | --- | --- |
| `id` | bigint PK | |
| `instance_host` | citext → `instance` | |
| `mastodon_id` | bigint | unique with host |
| `acct` | citext | `user@host` |
| `display_name` | text | may contain HTML; sanitized for storage |
| `note` | text | bio, **truncated to 2 KB**; the full field is not needed for ranking |
| `avatar_url`, `header_url` | text | URL only, no media |
| `bot`, `locked`, `discoverable` | boolean | |
| `follows_private` | boolean | `hide_collections`; if true we cannot expand this account |
| `followers_count`, `following_count`, `statuses_count` | integer | as reported at `last_fetched_at` |
| `last_status_at` | timestamptz NULL | activity recency, drives freshness scoring |
| `moved_to_account_id` | bigint NULL → `account` | after an instance move |
| `first_seen_at`, `last_fetched_at` | timestamptz | |
| `fetch_failures` | smallint | backoff hint |

Unique: `(instance_host, mastodon_id)`; index on `acct`; index on
`(last_status_at desc)` for activity filters.

### `follow_edge`
Who follows whom, as observed by us. This is the backbone of similarity.

| Column | Type | Notes |
| --- | --- | --- |
| `followee_id` | bigint → `account` | |
| `follower_id` | bigint → `account` | |
| `source` | smallint | `1` own-user sync, `2` 2-hop expansion, `3` observed via post metadata |
| `first_seen_at` | timestamptz | |
| `last_confirmed_at` | timestamptz | refreshed on re-sync; stale edges decay |

PK `(follower_id, followee_id)`; secondary index `(followee_id, follower_id)` for
reverse lookups (who follows X — needed for reasons and for similarity).

### `engagement_edge`
Observed engagement by our users. Private signals, never exposed per-user.

| Column | Type | Notes |
| --- | --- | --- |
| `actor_id` | bigint → `account` | must be an `app_user`'s account |
| `kind` | smallint | `1` favourite, `2` boost, `3` reply, `4` bookmark |
| `status_id` | bigint NULL → `status` | the engaged post; follows live in `follow_edge` |
| `target_account_id` | bigint → `account` | author of the engaged status |
| `created_at` | timestamptz | |
| `weight` | real | derived: favourite 1.0, boost 1.5, reply 2.0, bookmark 1.0 |

PK `(actor_id, kind, status_id)`; index `(target_account_id)`. Retention: 180
days (engagement is the highest-value signal; the shortest we would go is 90).

### `status`
Recent posts from candidate authors, only what the for-you feed needs.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | bigint PK | |
| `instance_host` / `mastodon_id` | citext / bigint | unique pair |
| `author_id` | bigint → `account` | |
| `created_at` | timestamptz | |
| `visibility` | smallint | public / unlisted only; anything else is dropped at ingest |
| `reblog_of_id` | bigint NULL → `status` | boosts are first-class rows |
| `reply_to_account_id` | bigint NULL | for reply penalties |
| `text` | text | **plaintext, truncated to 2 KB**, no HTML |
| `language` | text NULL | |
| `sensitive` | boolean | client shows a warning |
| `favourite_count`, `reblog_count`, `reply_count` | integer | engagement prior |
| `url` | text | canonical link back to the instance |
| `ingested_at` | timestamptz | |

Index `(author_id, created_at desc)`, index `(created_at desc)`. **Retention: 14
days** (`POST_RETENTION_DAYS`), refreshed by the P3 sync job.

### `user_similarity`
Similar users of our users (the product's core relation), recomputed nightly.

| Column | Type | Notes |
| --- | --- | --- |
| `app_user_id` | uuid → `app_user` | |
| `similar_app_user_id` | uuid → `app_user` | |
| `score` | real | 0..1, see [05-similarity.md](05-similarity.md) |
| `signals` | jsonb | `{co_follow: 12, co_fav: 4, co_reply: 1}` for explainability |
| `computed_at` | timestamptz | |

PK `(app_user_id, similar_app_user_id)`; keep **top 50 per user** only.

### `account_affinity`
Precomputed candidate scores per user, so a feed request is a read.

| Column | Type | Notes |
| --- | --- | --- |
| `app_user_id` | uuid → `app_user` | |
| `candidate_account_id` | bigint → `account` | |
| `score` | real | graph score before personal ranking |
| `reason_code` | smallint | `1` followed by similar users, `2` followed by 2-hop path, `3` popular fallback, `4` co-engagement |
| `support` | integer | how many similar users follow it (for "followed by N people you follow") |
| `computed_at` | timestamptz | |

PK `(app_user_id, candidate_account_id)`; index `(app_user_id, score desc)`.
**Keep top 500 per user.**

### `recommendation_item`
What we actually showed, so feedback can be attributed and explained.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | bigint PK | returned as an opaque item id |
| `app_user_id` | uuid → `app_user` | |
| `kind` | smallint | `1` account, `2` status |
| `account_id` | bigint NULL | |
| `status_id` | bigint NULL | |
| `rank` | smallint | position shown |
| `score` | real | |
| `model_version` | integer → `model_version` | |
| `reason` | jsonb | `{code, support, sample_followers: [...]}` (max 3 handles) |
| `shown_at` | timestamptz | |

Index `(app_user_id, shown_at desc)`. **Retention: 7 days** — beyond feedback
attribution we do not need it.

### `interaction_event`
Telemetry from the Feed tab. Only for `telemetry_opt_in` users.

| Column | Type | Notes |
| --- | --- | --- |
| `id` | bigint PK | |
| `app_user_id` | uuid → `app_user` | purge target |
| `item_id` | bigint NULL → `recommendation_item` | null for feed-level events |
| `kind` | smallint | `1` impression, `2` click, `3` follow, `4` dismiss, `5` not_interested, `6` boost, `7` reply |
| `dwell_ms` | integer NULL | client-measured, coarse buckets only |
| `created_at` | timestamptz | client clock, server-clamped |
| `received_at` | timestamptz | |

Index `(app_user_id, created_at desc)`, index `(item_id)`, index
`(created_at)` for the retention job. **Retention: 90 days.**

No partitioning in v1: at 1k users this table is ~3M rows / ~350 MB after 90
days, which a single index handles. Revisit at 10M rows — then monthly range
partitions with `DROP PARTITION` retention (see [decisions.md](decisions.md) D7).

### `job`
The work queue, drained with `FOR UPDATE SKIP LOCKED` (no broker).

| Column | Type | Notes |
| --- | --- | --- |
| `id` | bigint PK | |
| `kind` | smallint | `1` sync_user, `2` expand_follows, `3` refresh_account, `4` fetch_statuses, `5` compute_similarity |
| `payload` | jsonb | e.g. `{"account_id": 42}` |
| `priority` | smallint | P0..P4 |
| `run_after` | timestamptz | backoff / schedule |
| `attempts` | smallint | |
| `locked_at`, `locked_by` | timestamptz, text | lease, 60 s |
| `done_at` | timestamptz NULL | |
| `last_error` | text NULL | |

Index `(priority, run_after)` where not done. **Retention: delete done rows after
7 days.** `sync_cursor` keeps per-target progress (`following_page`, `favourites_max_id`,
`last_status_seen_at`) so jobs resume instead of restarting.

### `model_version`
Ranking weights are data, not constants.

| Column | Type | Notes |
| --- | --- | --- |
| `version` | integer PK | monotonic |
| `weights` | jsonb | named coefficients (`w_sim`, `w_support`, `w_fresh`, …) |
| `metrics` | jsonb | offline validation metrics when promoted |
| `created_at` | timestamptz | |
| `active` | boolean | exactly one row true |

## Size budget

Assumptions: 1,000 our users, 100,000 tracked accounts, expansion limited to
accounts followed by ≥ 2 users. This is the budget every feature must fit in.

| Table | Rows | Bytes/row incl. indexes | Size |
| --- | --- | --- | --- |
| `account` | 100k | ~900 | ~90 MB |
| `follow_edge` | 8M | ~48 | ~380 MB |
| `engagement_edge` | 2M | ~40 | ~80 MB |
| `status` (14-day retention) | 1M | ~700 | ~700 MB |
| `user_similarity` | 50k | ~80 | ~4 MB |
| `account_affinity` | 500k | ~60 | ~30 MB |
| `recommendation_item` (7 days) | 200k | ~150 | ~30 MB |
| `interaction_event` (90 days) | 3M | ~120 | ~360 MB |
| **total** | | | **~1.7 GB** |

Levers, in the order we would pull them: shorten `status` retention, lower
`account_affinity` top-K, drop `status.text` (fetch on demand), cap expansion to
accounts followed by ≥ 3 users, shorten interaction retention.

What we deliberately do **not** store: follower lists of arbitrary accounts
(explodes quadratically — follower counts only), media, full HTML bodies, DMs,
notification payloads, raw HTTP responses.

## Query shapes that must stay fast

- Candidate generation: "followees of the 50 most similar users" → `follow_edge`
  by `follower_id` (PK prefix), `user_similarity` by `app_user_id`.
- Reverse lookup for reasons: "who do I follow that also follows X" → index
  `(followee_id, follower_id)`.
- Post feed: "latest 20 statuses from these 500 author ids" → `(author_id, created_at desc)`.
- Frequency capping: "recent impressions for user" → `interaction_event`
  `(app_user_id, created_at desc)`.
- Retention: range delete on `created_at` — batched (`LIMIT 10_000` in a loop) so
  vacuum keeps up.
