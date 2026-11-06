# 04 — Ingestion

Status: draft · Last revised: 2026-11-06

Ingestion answers one question: **what is the cheapest set of requests that still
produces good candidates?** Mastodon's rate limits are generous per token and
strict per instance/IP, so the real budgets are politeness, DB growth and signal
quality — not the headline quota.

## Access model

There is no OAuth flow of ours (decision D4, revised). The **client** registers
itself with the instance (`POST /api/v1/apps`, PKCE `S256`) and keeps the token
in IndexedDB; we never see a redirect or a client secret. A user can then give
us a **copy** of that token for background work, or not:

| Mode | How the worker authenticates | What it enables |
| --- | --- | --- |
| `server_sync` (opt-in) | encrypted `app_user.access_token_enc` | full graph sync while the user is offline; neighbour expansion; P2–P4 tiers |
| `request_scoped` (default) | the bearer token attached to the feed request itself | ranking the requesting user only, with a 10-minute verify/fetch cache |

`request_scoped` is real, not a degraded stub: the API reads the user's following
and favourites live, caches them per token fingerprint, and ranks. What it cannot
do is expand two hops out (that requires crawling accounts the user never
requested) or refresh anything while the app is closed. Similarity therefore
advances only while the user is active, and the feed reports
`degraded: no_server_sync` so the UI can say so.

In both modes we read:

| Data | Endpoint | Scope | Why |
| --- | --- | --- | --- |
| Own account | `/api/v1/accounts/verify_credentials` | `read:accounts` | who the user is |
| Following list | `/api/v1/accounts/:id/following` | `read:follows` | primary taste signal |
| Followers | `/api/v1/accounts/:id/followers` | `read:follows` | reciprocity feature |
| Favourites | `/api/v1/favourites` | `read:favourites` | strongest engagement signal |
| Bookmarks | `/api/v1/bookmarks` | `read:bookmarks` | private save signal |
| Notifications | `/api/v1/notifications` | `read:notifications` | who they reply to / who follows them |
| Public statuses | `/api/v1/accounts/:id/statuses` | `read:statuses` | post feed and activity |
| Blocks / mutes | `/api/v1/blocks`, `/api/v1/mutes` | `read:blocks`, `read:mutes` | never recommend what the user silenced |
| Follow (optional) | `POST /api/v1/accounts/:id/follow` | `write:follows` | the client follows; we never proxy a follow |

`write:follows` appears only in the client's registration when the user enables
one-click following; our stored copy is never used to write. Following from the
Feed tab is performed by the browser against the instance directly, which keeps
us out of the write path and out of that audit trail entirely.

**Token handling.** In `server_sync` the copy is encrypted at rest with
XChaCha20-Poly1305 (`TOKEN_ENC_KEY`), stored as nonce‖ciphertext, decrypted only
in the request or job that needs it, never logged, never sent back to the
browser. The verify cache is keyed by `sha256(token)` — we never need the plain
token to look up a user. A 401 on any call marks the token revoked, stops that
user's sync, and surfaces a re-authorize prompt in the client; we do not retry
revoked tokens.

## Rate-limit budget

Mastodon defaults (per instance, configurable):

- **300 requests / 5 min per user token** → 1 req/s sustained
- **7,500 requests / 5 min per IP** → 25 req/s sustained
- `X-RateLimit-Limit` / `-Remaining` / `-Reset` headers are authoritative;
  we adapt down when an instance reports lower limits.

At 1,000 our users the per-token quota is effectively unlimited for our own
reads (~10 requests per user per day, versus 86,400 available). The binding
constraints, in order:

1. **Per-instance politeness.** We self-limit to **1 request/second per host**
   (burst 5) and never exceed 50% of the reported remaining budget. Being
   blocked by a large instance would end the product.
2. **IP reputation.** All requests come from one egress IP, so instances that
   limit by IP see us as a single client. Cap global concurrency at ~8 in flight.
3. **DB size.** Every expanded account costs ~200 follow edges and ~1 KB of
   metadata; the expansion cap is what keeps `follow_edge` in budget.
4. **Freshness value.** A three-week-old follow list is worth much less than
   today's; there is no point spending requests to refresh the long tail weekly.

### Per-request cost of one user's daily sync

| Step | Requests |
| --- | --- |
| `verify_credentials` | 1 |
| following (200 accounts / 80 per page) | 3 |
| followers (first 2 pages) | 2 |
| favourites (incremental via `min_id`) | 1–2 |
| bookmarks | 1 |
| notifications (mentions only, since last seen) | 1–2 |
| **total** | **~10 / day** |

## Sync tiers

The worker drains a `job` table (priority, `run_after`, lease via
`FOR UPDATE SKIP LOCKED`). Tiers exist so that a backlog degrades gracefully
instead of blocking the feed.

| Tier | Job | Cadence | Cost and cap |
| --- | --- | --- | --- |
| P0 | `sync_user` — everything in the table above | on login, then every 24 h | ~10 req/user; never skipped |
| P1 | `expand_follows` — fetch following list of an account followed by ≥ 2 users | hourly sweep, 500 accounts/sweep | 1 req/account; hard cap `EXPANSION_MAX_ACCOUNTS` (default 20k) |
| P2 | `refresh_account` — metadata for tracked accounts | 7-day TTL, popularity-weighted | 1 req/account, ~15k/day at 100k tracked |
| P3 | `fetch_statuses` — last 20 statuses of top candidate authors | every 6 h | 1 req/author, ~5k per sweep |
| P4 | long-tail crawl / backlog | best effort | fills idle budget only |

**Expansion policy.** BFS from the union of our users' followees, ordered by
*support* (how many of our users follow the account). Stop expanding an account
when: it is `follows_private`, it is a bot (`bot && !user_wants_bots`), it has
not posted in 90 days, its instance is on our blocklist, or the global cap is
reached. Depth is **2 hops** — deeper is where quality collapses and volume
explodes.

**Status ingestion.** Only for authors who are actual candidates (top of
`account_affinity`), only public/unlisted visibility, only the last 20 posts,
re-fetched every 6 h. Boosts are stored as rows referencing the original status;
the original status metadata comes from the boost payload, so no extra request.

**Streaming.** Mastodon's streaming API is not used in v1. It would give fresher
posts but requires a persistent connection per instance and a token with
`read:statuses`; the 6-hour polling window is enough for a discovery feed. Revisit
if post freshness becomes a product complaint.

## Backoff and failure handling

| Response | Action |
| --- | --- |
| 429 | honour `Retry-After`, exponential backoff with jitter (1s → 1h), lower per-host concurrency for 10 min |
| 5xx | exponential backoff (1s → 30min), `fetch_failures += 1` |
| 401 | token revoked → mark, stop, prompt user; never retry |
| 403 / 404 | account gone/limited → mark `unavailable_at`, remove from candidates, stop expansion |
| 410 | treat as gone |
| network error | backoff; do not count as a token failure |

A per-host token bucket lives in the worker (`dashmap` or a small `moka` cache:
host → next-allowed instant). Jobs for a host that is currently limited are
re-scheduled rather than retried in place.

## Federation edge cases

- **Moved accounts** (`moved` field): record `moved_to_account_id`, merge edges
  onto the target at the next similarity pass, stop ingesting the old account.
- **`hide_collections`**: following/follower lists are private. Mark
  `follows_private = true`, keep the account as a candidate (we can still see its
  posts and counts), never attempt expansion.
- **Instance-local ids**: a Mastodon account id is only meaningful on its own
  host. Identity is always `(instance_host, mastodon_id)`; `acct` is for display
  and dedupe hints.
- **Duplicated people**: one person, several accounts (main + alt + moved).
  `acct` stays distinct; similarity treats them as different accounts unless a
  `moved_to` link exists. Do not attempt fuzzy identity merging in v1.
- **Silenced/limited instances**: posts may arrive but be limited on the origin.
  Treat visibility as reported; do not try to infer instance policy.
- **Instance blocklist**: `BLOCKED_INSTANCES` (comma-separated) removes hosts
  from crawl and candidates. Moderation is a product requirement, not a detail.
- **Very large instances** (mastodon.social, etc.): cap tracked accounts per host
  (`MAX_ACCOUNTS_PER_HOST`, default 5,000) so one host cannot consume the budget.

## Data quality filters

Applied when writing candidates, not when ingesting (we still want the edge):

- self, already followed, blocked, muted → excluded from candidate lists
- `bot` → excluded unless the user follows ≥ 2 bots already (then bots are clearly wanted)
- no posts in 90 days (`last_status_at`) → excluded
- zero followers (`followers_count = 0`) → excluded, usually a dead or test account
- accounts that only boost (no original posts) → allowed, but scored down
- accounts whose display name/note match known spam patterns → excluded (blocklist-driven)

## What ingestion must never do

- Fetch or store DMs, notification contents, media, or non-public statuses.
- Use another user's token for a user's private reads (favourites, bookmarks,
  notifications). Crawling public data with a rotating token is acceptable;
  **private reads are always with the owner's token.**
- Exceed 50% of an instance's reported remaining budget.
- Retry a revoked token, or crawl a host marked `blocked`.
- Write anything outside `migrations/` that changes schema.
