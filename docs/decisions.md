# Decisions

Status: draft · Last revised: 2026-11-06

One row per irreversible-or-expensive choice, with the reason it was made and
what would have to be true to revisit it. Numbers are stable so the other
documents can cite them (`D4`).

| # | Decision | Reason | Revisit if |
| --- | --- | --- | --- |
| D1 | **sqlx, not Diesel** | the interesting queries are analytical (co-follow joins, IDF aggregation); async-native; compile-checked SQL without a DSL to fight | we need a query builder for dynamic filters; then add `sea-query` for those paths only |
| D2 | **Docker-outside-of-Docker**, not DinD | the host's Docker data root is on overlayfs, where nested overlayfs fails; DooD uses the host daemon directly | we move to a host with a sane data root and need isolation between projects |
| D3 | **A dedicated `store` crate** | one place that writes SQL; `core` stays pure and testable without a database | never, really — this is the boundary that keeps ranking testable |
| D4 | **Browser-held Mastodon token, server copy opt-in** | the client must talk to instances directly to be worth being a client; a proxy doubles rate-limit exposure; a copy is only needed for background sync | instance APIs make background sync unnecessary (e.g. push from the instance) |
| D5 | **Leptos, not Yew** | fine-grained signals suit feed virtualization better than VDOM diffing; `view!` is close enough to JSX for anyone who knows React; 0.8 is stable and CSR-friendly | a hostile 0.9 migration, or a11y/ecosystem gaps we cannot close |
| D6 | **Full client, not just a Feed tab** | a Feed tab bolted onto Mastodon's web UI would require forking Mastodon or a browser extension; a client is the only honest delivery vehicle | the pilot shows nobody switches clients — then the Feed becomes an embedded surface again |
| D7 | **No partitioning in v1** | 8M `follow_edge` rows and 10M events fit comfortably; partitioning adds operational cost before it buys anything | `interaction_event` > 50M rows, or vacuum pressure on `status` |
| D8 | **CSR, no SSR** | every meaningful render needs the browser's token; SSR would render shells and add a server to the critical path | we add an unauthenticated public surface (e.g. a landing page worth SEO) — then only that route prerenders |
| D9 | **Postgres job queue, no broker** | one dependency fewer; `FOR UPDATE SKIP LOCKED` is enough at 1 req/s per host; the claim query is 15 lines | job volume > 100/min, or we need fan-out across many workers |
| D10 | **Separate `sdk` repository** | two deployables on two cadences need a versioned wire contract and a CI that compiles both against a candidate commit; shared DTOs are the whole point of doing this in Rust | the contract stabilises so much that a monorepo's atomic changes win — unlikely while both sides ship |
| D11 | **Self-imposed 50% rate-limit budget** | politeness is the constraint that keeps us from being blocked, and 429 storms are worse than slow sync | never; raise throughput by adding users, not by spending more of the quota |
| D12 | **No telemetry vendor, no email collected** | the privacy story is a feature and an audit surface we do not want; `interaction_event` is opt-in and first-party | operational needs (crash reporting) exceed what logs give us |
| D13 | **The SDK owns client state** | the client is a renderer: session, timelines, feed, streaming status and cache policy live in `nodus-sdk` behind a `Sdk<I, D, P, C>` over four ports; otherwise pagination and cache rules would exist once in the browser and again in the sync worker and drift | a second UI (native, terminal) appears — the split is the same, only the renderer changes |
| D14 | **Superproject with submodules, SDK consumed by path** | one gitlink per project pins a known-good combination; `path = "../sdk/crates/…"` keeps cross-project edits atomic and buildable before pushing, and avoids a second copy of the sources in `~/.cargo/git` | an external consumer appears outside this superproject, at which point a tagged `git =` dependency becomes the right second channel |
| D16 | **Feeds and calculator are separate deployables** | the two workloads fail differently and scale on different signals: a feed request is one indexed read plus O(500) arithmetic, while similarity, affinity and model fitting grow with the whole graph. One process would let a recompute steal pool capacity and CPU from user requests — the two failures this split exists to prevent | a single binary stays under a few hundred users *and* has no recompute job of its own |
| D17 | **Redis for ephemeral shared state, degradable** | N feeds replicas need one verification cache, and M calculator replicas need one per-host budget; both are caches whose loss costs a few extra Mastodon requests, never correctness. The alternative — a `rate_bucket` table — makes every request-budget check a write to the primary | Postgres-side coordination proves cheaper than the extra dependency, or Redis becomes a storage engine |
| D18 | **Whole-graph jobs take a distributed lock; per-item jobs do not** | `recompute_similarity`/`recompute_affinity`/`fit_model` are idempotent but expensive, so exactly one replica runs each; the P0–P4 crawl jobs are already partitioned by `SKIP LOCKED` and need no lock. Leases and TTLs make a dead owner self-healing without an elected leader | job volume or graph size makes a full recompute exceed one lease TTL often enough to lose progress |
| D19 | **sqlx stays; `diesel-async` is not a performance option** | the driver is not the model: `diesel-async` runs Diesel's query builder over a non-blocking connection with the same await-a-socket behaviour, so both are equal at the database and the real lever is the query plan. We would pay a rewrite of six hot statements (`SKIP LOCKED` claim, `UNNEST` batch upsert, `ON CONFLICT … WHERE … RETURNING xmax = 0`, `ctid`-batched retention, dynamic feed filters) into `sql_query`, ending with two ways to write SQL and a compile-time guarantee that stops exactly where the risk starts | a second writer surface with ordinary CRUD (an admin UI) makes Diesel's schema typing worth its escape hatches; see the note below |
| D20 | **Ingestion is shared and per-account, never per-user** | two users who follow the same popular account must not produce two copies of it, or the database grows with the square of the audience. Every fetch is keyed on `(instance_host, mastodon_id)` and stamped in `account.last_fetched_at`; a second user's sync reads what is already there and only fetches accounts past their TTL (`refresh_account`, 7 days). Per-user data is only what is genuinely per-user: the edges, the events, the affinities | a per-user view of a *private* resource (follower-only replies, a poll vote) needs its own fetch, because that data genuinely differs per caller |
| D21 | **Redis is a cache and a lock, never a store** | the four keys it serves (`verify:`, `budget:`, `lock:`, `feed:`) all have a documented cold-start behaviour, so a flushed Redis costs latency and a burst of Mastodon requests, never correctness. Anything that would be lost by `FLUSHALL` belongs in Postgres. In-memory implementations of all three traits exist and are used when `REDIS_URL` is unset, which is what keeps single-replica development dependency-free | a fourth use appears that cannot be rebuilt from Postgres, at which point it is storage and the answer changes |
| D22 | **No embedding vectors in v1; the scalar schema is the extension point** | `user_similarity` and `account_affinity` store real-valued scores with their components (`co_follow`, `co_fav`, `co_reply`, `reciprocity`) rather than a vector, because every score is explainable to the user who sees it — which is a product requirement, not a preference. `pgvector` is installed and the migration path is additive (a `vector(N)` column plus an HNSW index), so the option stays open without paying for a model, an embedding pipeline and an unexplainable score today | a content signal (post text, topics) is needed to recommend across instances with no follow overlap, and the cold-start ladder cannot cover it with popularity |
| D23 | **Crawl accounts, not users; schedule tokens** | once refresh runs continuously, the unit of work is the account — 500 users following one account is one series of fetches, not 500. So the queue holds per-account work and the token is a *resource* the worker schedules: any consented token from an instance can fetch, preferring the one with the most remaining budget and the oldest last use. `sync_user` shrinks to ingesting that user's edges; it no longer owns status fetching | a single instance's token pool is too thin to crawl it (one consenter, many accounts), at which point crawling falls back to that user's own budget and the graph stays shallow there |
| D24 | **Refresh policy: temperature tiers, 3 pages max, priority = audience weight** | an account's refresh interval is decided by how many people its posts can reach: `hot` (in ≥ 25 users' affinity sets, or followed in the last 7 days) every 6 h; `warm` (in ≥ 1) every 24 h; `cold` (known but in nobody's set) every 7 days, and only the first page. Page depth follows the same tier — 1 page (40 posts) cold, 2 warm, 3 hot — because `POST_MAX_AGE_DAYS = 7` means a fourth page is never shown. Ordering inside a tier is `audience_weight DESC, last_refreshed_at ASC`, and a host whose budget is spent reschedules with jitter rather than failing, so a throttled host does not produce a retry storm | measurement shows the 7-day post window rarely binds and a deeper first fetch is worth more than a second one |


| D15 | **No Mastodon mark in any name** | the trademark policy forbids the word mark in a product, service or company name, and the software licence grants no licence to the marks; the project is `Nodus`, crates are `nodus-*`, and the mark appears only in sentences that state compatibility (see [11-trademark.md](11-trademark.md)) | never; this is a legal constraint, not a preference |

## Notes on the ones that cost the most

**D4** is the load-bearing privacy decision. It means our server never sees the
user's timeline traffic, which is both the strongest privacy property and the
reason `request_scoped` mode can work at all. The cost is honest: the worker
cannot crawl for a user who has not consented, so similarity for those users is
weaker and slower. We chose to expose that as `degraded: no_server_sync` rather
than hide it.

**D6** is the scope decision. It turns a recommendation service into a product
with a client — months of work, a11y and i18n obligations, and competition with
Elk and Phanpy. It also makes the Feed tab possible without forking Mastodon,
which was the alternative we rejected.

**D10** is the one most likely to be second-guessed. Splitting three repositories
buys independent deploy cadence and a hard, versioned boundary; it costs atomic
changes across the wire. The contract CI (see
[03-contract.md](../../sdk/docs/03-contract.md))
exists specifically to make the cost visible early.

**D15** cost a rename of every repository, crate and binary shortly after the
first push, which is the cheapest possible moment: no downstream consumer existed,
GitHub keeps redirects from the old names, and the git histories were preserved.
The lesson is to check naming constraints before publishing, not after.

## Superseded

| # | Was | Now | Why |
| --- | --- | --- | --- |
| D4a | one per-instance dynamically registered `oauth_app`, server-side PKCE sessions | client registers itself; server optionally holds a token copy | a full client owns the OAuth flow; keeping a second registration would mean two identities per instance |
| D8a | standalone web app for the Feed tab, SSR-capable | full CSR client | a standalone Feed app is not a client; the Feed must live where timelines do |
| D10a | project named `MastodonReccom`, crates `mastodon-reccom-*` | `Nodus`, crates `nodus-*` | the mark may not appear in a product or service name (D15) |
### Why not `diesel-async` (D19 in detail)

The question is fair: `diesel-async 0.9` exists, `diesel 2.3` is mature, and "async
Diesel" sounds like Diesel with sqlx's concurrency. It is not, and the difference
matters less than it looks:

* **The driver is not what makes a query asynchronous.** Both crates end up waiting
  on a socket from a tokio task. `diesel-async` adds a non-blocking connection pool
  (`deadpool` or `bb8`) in front of Diesel's builder; sqlx awaits directly. Round-trip
  throughput is the same, and the Twitter-benchmark-era folklore about "slow ORMs"
  was about *query plans and round trips*, not about the Rust layer above libpq.
* **Async is not the axis where we gain.** The measured tail in `10-roadmap.md` is
  the calculator's own work (similarity, affinity), which is CPU, not database. On the
  feeds side the budget is one indexed read; changing the driver cannot move it.
* **The compile-time guarantee stops where our risk starts.** Diesel checks the
  queries it can express. These are the queries that actually decide whether the
  service holds up, and none of them is expressible as a Diesel builder:

  | Statement (in `crates/store`) | Why the DSL cannot express it |
  | --- | --- |
  | `jobs::claim_kinds` | `WITH due AS (… FOR UPDATE SKIP LOCKED) UPDATE job … RETURNING job.*` — a CTE around a locking update |
  | `accounts::upsert_many` | one `UNNEST($1::text[], $2::bigint[], …)` instead of N bound tuples, which is what keeps a 5 000-row crawl to one round trip |
  | `jobs::enqueue` | `ON CONFLICT (kind, subject_kind, subject_id) WHERE status IN (…) DO UPDATE … RETURNING id, (xmax = 0) AS inserted` — the conflict target is partial and the output includes a system column |
  | `retention::retain_batch` | `DELETE … WHERE ctid IN (SELECT ctid … LIMIT n)`, repeated until the row count drops |
  | `accounts::replace_follows` | delete-then-insert inside one transaction with array binds |
  | feed query | eight optional filters over a keyset cursor |

  In Diesel each becomes `sql_query(...)` plus a `QueryableByName` struct, so we
  would keep sqlx-shaped SQL *and* add a second layer that no longer checks it.
* **Two migrations tools, two notions of the schema.** Our migrations are plain SQL
  applied by `sqlx::migrate!`, and `crates/store/migrations/0001_initial.sql` is
  reviewed as SQL. Diesel wants `diesel print-schema`-generated modules or
  `diesel_migrations`; either way the schema acquires a second description that can
  disagree with the SQL.
* **Small frictions that are only small until they are not.** `citext` has no Diesel
  core type (we already select it `::text` in sqlx); `time`/`uuid` mappings differ;
  every optional filter on the feed query becomes a boxed select statement, which is
  the same amount of code as the SQL string.

**What would change the answer.** A second writer with ordinary CRUD — an admin UI
over `instance`, `app_user`, `job` — is exactly the shape Diesel is good at, and it
would justify keeping a `diesel` module next to the SQL one for *those* tables. Until
then the honest summary is: same performance, less of our code covered, one more
description of the schema. Recorded as D19 so the question is answered once.
