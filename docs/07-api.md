# 07 — HTTP API

Status: draft · Last revised: 2026-11-06 (revision 3: bearer auth, contract versioning, CDN-hosted client)

The API is the **discovery plane** for the Leptos client and any future client.
It is the only writer of `recommendation_item` and `interaction_event`, and it
never proxies Mastodon traffic — timelines and compose go from the browser
straight to the instance.

Base path: `/api/v1`. JSON in, JSON out, `snake_case` fields, RFC 3339
timestamps. Errors use a single shape:

```json
{ "error": { "code": "rate_limited", "message": "Too many feed requests", "retry_after_s": 12 } }
```

| Status | `code` values |
| --- | --- |
| 400 | `invalid_request`, `contract_too_old` |
| 401 | `unauthenticated`, `token_revoked` |
| 403 | `consent_required`, `blocked_instance` |
| 404 | `not_found` |
| 426 | `client_too_old` (server no longer accepts this contract) |
| 429 | `rate_limited` |
| 502 | `upstream_error` (Mastodon call failed) |

## Auth

**Bearer Mastodon access token. No sessions, no cookies, no OAuth endpoints of
our own.** The client obtains the token itself (dynamic app registration + PKCE
against its instance, token in IndexedDB — see
[02-architecture.md](../../sdk/docs/02-architecture.md)) and presents it on every call:

```http
Authorization: Bearer <mastodon_access_token>
```

The server validates by calling `/api/v1/accounts/verify_credentials` once,
then caches `sha256(token) → (app_user_id, instance, scopes)` for 10 minutes, so
a feed request costs no Mastodon round trip. A `401` from the instance is
returned as `token_revoked` and the mapping is dropped.

Consequences, stated plainly:

- Tokens are long-lived (Mastodon does not expire them). The server stores a
  copy **only if** the user grants background sync; see
  [08-privacy.md](08-privacy.md).
- No CSRF surface, no cookie `SameSite` reasoning, no session table.
- Abuse limiting is per token hash (`sha256` prefix), never per IP alone.

### Scopes

| Scope | Needed for |
| --- | --- |
| `read:accounts` | verify, following lists |
| `read:favourites`, `read:bookmarks`, `read:notifications` | signal sync |
| `read:statuses` | post signals |
| `write:follows` | optional: enable follow from inside the client's Feed tab |

The client requests exactly these. Anything missing makes the corresponding
signal unavailable rather than failing the request; the response says which
signals are degraded (`signals_unavailable: ["favourites"]`).

## Contract versioning

The wire types live in `nodus-shared` (the contract repo), which is
versioned independently of this service and of the client. Rules:

1. **Additive only within a minor.** New fields are optional with `serde(default)`;
   fields are never removed or renamed inside a minor. A rename is a breaking
   change and a new major of the contract crate.
2. The API advertises what it speaks and accepts, publicly and without auth:

```http
GET /api/v1/meta
```
```json
{
  "api": "1",
  "contract": "0.4.0",
  "min_contract": "0.3.0",
  "features": ["feed.posts", "feed.explain", "sync.token", "events.batch"]
}
```

3. The client sends `X-Contract-Version: 0.4.0` on every request. Older than
   `min_contract` → `426 client_too_old`; the client shows a hard
   "update required" screen instead of silently misreading fields.
4. `features` is the capability list the client gates UI on (no version
   sniffing in the client).

## Background sync consent

### `PUT /api/v1/sync/token`

```json
{ "enable": true, "telemetry_opt_in": false }
```

Verifies the token (see Auth), encrypts it (XChaCha20-Poly1305, key from
`TOKEN_ENC_KEY`), stores it on `app_user`, and enqueues P0 sync. Idempotent.
The client calls this only after an explicit consent screen and can revoke it
without losing its own access:

### `DELETE /api/v1/sync/token`

Stops server-side sync, wipes the stored token and the user's derived data
(similarity, affinity) immediately; the client keeps working because it holds its
own token. This is the opt-out path and must never break the Feed tab — the API
falls back to ranking on demand for that user.

## Feed

Both endpoints rank on demand, persist the shown snapshot, and return an opaque
cursor. Page 2 reuses the `feed_id` snapshot so impressions and clicks map to
stable item ids.

### `GET /api/v1/feed/accounts?limit=20`

```json
{
  "feed_id": "01JABCDEF...",
  "generated_at": "2026-11-06T21:04:00Z",
  "contract": "0.4.0",
  "model_version": 7,
  "similarity_age_hours": 3.2,
  "degraded": null,
  "signals_unavailable": [],
  "items": [
    {
      "item_id": 918273,
      "account": {
        "id": "0193f0...",
        "acct": "alice@example.social",
        "display_name": "Alice",
        "note": "distributed systems, cats",
        "avatar_url": "https://example.social/avatars/...",
        "followers_count": 1204,
        "last_status_at": "2026-11-06T09:12:00Z",
        "bot": false
      },
      "reason": {
        "code": "followed_by_similar_users",
        "support": 4,
        "sample_followers": ["bob@x.social", "carol@y.social"]
      },
      "already_following": false
    }
  ],
  "next_cursor": "eyJzIjowLjgyLCJpIjo5MTgyNzN9"
}
```

`degraded` is `null` normally, or `"cold_start"` / `"stale_similarity"` /
`"no_server_sync"` so the client shows an honest banner instead of faking
personalization.

### `GET /api/v1/feed/posts?limit=20&cursor=...`

Same envelope; items carry `status` instead of `account`:

```json
{
  "item_id": 918274,
  "status": {
    "id": "0193f1...",
    "account": { "id": "0193f0...", "acct": "alice@example.social", "display_name": "Alice" },
    "created_at": "2026-11-06T18:02:00Z",
    "text": "plain text, HTML stripped, 2 KB max",
    "sensitive": false,
    "favourite_count": 12,
    "reblog_count": 3,
    "reply_count": 5,
    "url": "https://example.social/@alice/0193f1"
  },
  "reason": { "code": "author_you_should_follow", "via": "alice@example.social" }
}
```

Statuses from the feed are rendered by the client itself (it has the account
and the post; it does not need to re-fetch each one). Media, emoji and full
HTML are **not** included: the feed carries text, the client enriches on demand
via the instance when the user opens a post.

### `POST /api/v1/feed/:feed_id/refresh`

Explicit re-rank (pull-to-refresh). Rate-limited to 1 per 30 s per token; returns
a new `feed_id`.

## Feedback

### `POST /api/v1/events`

Batched, idempotent:

```json
{
  "feed_id": "01JABCDEF...",
  "events": [
    { "item_id": 918273, "kind": "impression", "dwell_ms": 900, "client_ts": "2026-11-06T21:04:11Z" },
    { "item_id": 918273, "kind": "click", "client_ts": "2026-11-06T21:04:14Z" }
  ]
}
```

- `kind` ∈ `impression | click | follow | dismiss | not_interested | boost | reply`
- Max 100 events per request; server clamps `client_ts` to ±10 min of now.
- Accepted and discarded (204) for users without `telemetry_opt_in` — the client
  is never told to behave differently, so opt-out stays a server-side gate.
- Idempotency key = hash of `(item_id, kind, client_ts)`; duplicates are dropped.
- A `follow` event removes the account from the user's affinity list and
  enqueues a P0 sync, which is how the "followed from the feed" loop closes
  without a follow-proxy endpoint.

Response: `204 No Content`.

### `POST /api/v1/feedback/not-interested`

```json
{ "account_id": "0193f0..." }
```

Permanent per-user suppression; also drops the account's posts from that user's
future feeds.

## Account and settings

| Endpoint | Purpose |
| --- | --- |
| `GET /api/v1/me` | user, instance, granted scopes, telemetry opt-in, server-sync on/off, signal freshness |
| `PATCH /api/v1/me/settings` | `{ "telemetry_opt_in": true }`, language preferences |
| `DELETE /api/v1/me` | stop sync, revoke token at the instance, purge telemetry and affinity, soft-delete user (30-day hard purge) |
| `GET /api/v1/me/export` | JSON export of what we hold (own follows, engagements, events) |

`DELETE /api/v1/me` is the only endpoint that calls the instance's
`POST /oauth/revoke` on the user's behalf; `DELETE /api/v1/sync/token` never
revokes (the client still needs its token).

## Operational endpoints

| Endpoint | Notes |
| --- | --- |
| `GET /healthz` | liveness, no DB |
| `GET /readyz` | DB ping + migration state + worker heartbeat age |
| `GET /metrics` | Prometheus text; internal only, never public |

## CORS

The client is a static bundle on a CDN, so it is a different origin from the
API. Allowed origins come from `WEB_ORIGINS` (comma-separated, exact match):

```http
Access-Control-Allow-Origin: https://feed.example.org
Access-Control-Allow-Methods: GET, POST, PUT, PATCH, DELETE, OPTIONS
Access-Control-Allow-Headers: authorization, content-type, x-contract-version, x-request-id
Access-Control-Max-Age: 600
```

No `Access-Control-Allow-Credentials` (there are no cookies), no wildcard, and
`Vary: Origin`. Removing an origin from `WEB_ORIGINS` is a one-line rollback.

## Conventions

- Pagination is keyset-only (`next_cursor` is base64 of `{score, id}`); no
  offsets, so paging is stable while the underlying set changes.
- Limits: `limit` ∈ 1..50, default 20.
- All responses are `Cache-Control: no-store` except `/meta` (60 s).
- Request ids: `X-Request-Id` echoed in logs and error bodies.
- Rate limits per token hash: 30/min `feed/*`, 120/min `events`, 10/min
  `refresh`, 60/min everything else. Exceeding returns 429 with `retry_after_s`.
  The limiter is the only shared state between API instances (Postgres or a
  single-instance in-memory counter until we run more than one).
- The API never returns scores, weights, or other users' private data. `reason`
  is the deliberate, limited exception, capped at 3 sample handles.
