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
| D8 | **CSR, no SSR — except the shared-post page** | every signed-in render needs the browser's token, so SSR would render shells and put a server on the critical path for nothing. The one unauthenticated surface is a shared post link, and a chat client that fetches a URL never runs wasm: `/s/{instance}/{status}` is rendered as static HTML (meta tags plus the post text, no script) and cached. That is a landing page, not a second rendering path for the app | a public surface that needs interactivity — then only that route grows a hydration boundary; the app stays CSR |
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
| D25 | **A feed is one immutable snapshot per reader, shared across tabs** | `feed_id` names a fixed ordered item list plus the clock it was ranked at; backfill replays it, a second tab joins it while it is younger than `FEED_SNAPSHOT_TTL` (10 min), and only `POST /refresh`, expiry or a new day mints a new one. Time-dependent terms use the snapshot's `generated_at`, never `now()`. Feed depth is capped at `FEED_MAX_ITEMS` (500) and the client shows a terminal card rather than receiving a list that would have to be recomputed; a snapshot lives `FEED_TTL` (24 h) and then answers `410 feed_expired`, so the item table stays bounded by a day of impressions | a recomputed feed disagrees with itself mid-scroll, and two tabs showing two different lists reads as the app lying; if a deeper feed is ever needed the snapshot is extended by an append pass from the frozen clock, never re-ranked |
| D30 | **Similarity components are renormalised over what is measurable** | a component that cannot be observed for a neighbour kind must leave the denominator as well as the numerator: references can reach at most ~0.90 of the weight (no favourites, no reciprocity) while our own users reach 1.0, so dividing by the full weight sum would under-score every reference and let the top-50 cut prefer user neighbours for a measurement artefact. `co_boost` joins the components because a boost is a published status and therefore crawlable for anybody — it restores engagement evidence exactly where favourites are unavailable. `co_fav`'s share drops to 0.10 for the same reason | a reference population with real engagement data appears (a federated index we are allowed to read), at which point the renormalisation stays but the weights are re-derived from measured coverage |
| D26 | **Rejections decay; nothing is excluded forever** | a rejection is stored as `(strength, recorded_at)` and read back as `strength · 0.5^(age / half_life)` — 7 days for `dismiss`, 30 for `not_interested` — then shrunk by `n / (n + 3)` over that user's impressions of the account. Two consequences follow, and both are requirements rather than details: the candidate query filters on a decayed value instead of a boolean flag, and the explore channel must keep sampling accounts whose penalty is decaying, or the decay is decorative because no positive evidence can ever lift it. Training uses time-decayed sample weights so last year's dismissals cannot outvote this month's behaviour | the harness shows a rejection that decays produces the "why is this back" complaint more often than it recovers a changed mind, in which case the half-life lengthens rather than the mechanism being withdrawn |
| D27 | **Corrections are shrunk by evidence** | every inferred correction (attention, penalties, exploration rate) is multiplied by `n / (n + prior)` in `nodus-core`, with `prior = 25` impressions for a user and `3` for an account. Three impressions move a score by 11% of a correction's strength; two hundred move it by 89%. The alternative — full-strength corrections from the first impression — makes a new user's feed maximally malleable, which is backwards: thin evidence should produce a quiet feed, not a responsive one. Explicit instructions (`follow`, `dismiss`, `not_interested`) are deliberately *not* shrunk, because they are commands rather than estimates; their inertia lives in the half-life | a user complains that nothing adapts fast enough, at which point the prior lowers for that cohort rather than the mechanism being removed |
| D28 | **Attention is a correction, never a label** | `dwell_ms` adjusts a label by at most ±0.5 (half a click) around the user's own median dwell, and is ignored entirely when unmeasured or unconsented. A dwell label of its own would be wrong three ways at once: it conflates reading with leaving a tab open, it is unavailable for users who decline the separate attention consent, and it would let passive behaviour outvote an explicit action. The client only reports dwell for items that were rendered at least 250 ms with the tab visible and focused, and a 120-second cap keeps an overnight tab out of the data | dwell proves to be predictive enough to deserve a weight of its own, measured by the harness against the click label — the cap and the visibility gate stay regardless |
| D29 | **Popularity is a denominator, never a numerator** | `favourite_count` survives only as `w_status = 1/ln(e + favourite_count)`, which discounts redundant evidence; it never becomes a quality term. A `log1p(fav + 2·boost + 2·reply)` prior ranks what everyone already liked, which is the behaviour this project exists to replace, and it dominates sparse personalised signals precisely where personalisation matters most. `engagement_prior` therefore counts *attributed* engagement by similar accounts, and the popularity rung is labelled as unpersonalised in the UI instead of being quietly ranked as if it were good | attributed engagement turns out to be too sparse to fill a feed, in which case popularity returns as an explicitly labelled exploration channel with its own weight, not smuggled into the score |


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

## Notes

### Why the shared-post page is rendered by the API role (D8 in detail)

A link pasted into a chat client is fetched once by a crawler that executes no
JavaScript, so the only way for a post to unfurl is for the URL to answer with
real HTML containing the tags. Two ways to get that without giving up CSR:

1. **Prerender the whole client** — that is what D8 rejected: a Node or Leptos SSR
server on the critical path, rendering components whose data requires a token it
does not have.
2. **Render one page, without JavaScript** — what we do. `/s/{instance}/{status}`
   asks the instance for the public status (unauthenticated, so the instance's own
   visibility rules apply), reduces it to title, text, image and canonical URL, and
   emits a small document with `og:`/`twitter:` tags and a readable copy of the
   post. No script, no hydration, no bundle.

The page is cached under `og:v1:{host}:{id}` for `OG_CACHE_SECONDS` (a day), with
failures remembered for five minutes under the same key. A post shared into a busy
channel therefore costs one instance request per day instead of one per preview,
and a link to a followers-only post costs one 404 per five minutes. Cache is
`Cache-Control: public, max-age=300`, so the page can also sit behind a CDN.

Two properties are deliberate and worth keeping:

* the page says nothing about *why* a post is unavailable — private, deleted and
  unreachable are the same page, because the difference tells a stranger whether a
  post exists;
* a non-public `visibility` is refused even though an unauthenticated request
  should never return one: the check exists so a misconfigured instance cannot make
  our page publish a followers-only post.

## Superseded

| # | Was | Now | Why |
| --- | --- | --- | --- |
| D4a | one per-instance dynamically registered `oauth_app`, server-side PKCE sessions | client registers itself; server optionally holds a token copy | a full client owns the OAuth flow; keeping a second registration would mean two identities per instance |
| D8a | standalone web app for the Feed tab, SSR-capable | full CSR client | a standalone Feed app is not a client; the Feed must live where timelines do |
| D10a | project named `MastodonReccom`, crates `mastodon-reccom-*` | `Nodus`, crates `nodus-*` | the mark may not appear in a product or service name (D15) |
| D35 | **The graph hop is produced, and an engaged bridge is promoted** | the graph hop was documented but never computed, so a young instance could not fill a feed from follows alone; it now runs in the similarity pass as a second candidate source. The damping is not constant: if the reader has interacted with the bridge account, the path through it is evidence about their taste rather than about who is popular nearby, so it carries `0.6 · w_follow(i) · w_follow(c)` instead of the documented `0.25`. Scores and support from both sources add, and `account_affinity.sources` records which generator contributed, so the feed can explain a candidate without recomputing the graph | a candidate's two paths stop being independent evidence (for example if every engagement implies a follow), at which point the sum must be capped instead of added |
| D36 | **An impression is written on unmount, carrying dwell** | The impression is the denominator of every rate and the only evidence that lets a rejection decay (D26), so it must be written exactly once per item. The client reports it when the card leaves the screen, with the time it spent there, because that is the only moment dwell is known and the server deduplicates on `(item_id, kind, client_ts)` — a second report would double the denominator rather than correct it. The accepted cost is a small hole at the tail of the last page: an item visible when the tab closes is counted only if the page-hide flush reaches the server | the hole grows large enough to distort follow-through, or a `View` event kind earns its cost (D7) |

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

### Why the feed is a snapshot (D25 in detail)

The instruction that produced this decision was simple — "one page load is an
infinite feed, and once initiated it has to be consistent, even when a new feed
opens on another tab" — and it rules out the obvious implementation, which is to
rank on every request and let the client paginate.

Ranking on every request means the list is a function of `now()`. Two tabs then
disagree, a scroll re-reads a list that has changed underneath it, and an
impression recorded at item 40 may name a row that no longer exists. Ranking once
per snapshot moves the cost from "every page" to "once per ten minutes", and it
makes every item id a durable fact.

The cap at 500 is the honest part. An infinite feed implies an unbounded supply
of good candidates, and at our scale there isn't one: past a few hundred accounts
the ranking is recommending neighbours of neighbours with no evidence behind them.
A terminal card ("that's everyone we can suggest right now — check back after your
follows refresh") is more truthful than a spinner, and it keeps the table bounded
by `users × 500` rather than `users × pages scrolled × forever`.

### Why a snapshot dies after a day (D25, the TTL half)

A snapshot that never expires is a table that grows with every scroll of every
user, and its cursor is a promise that cannot be kept once the rows are gone. The
24-hour ceiling is where those two facts meet: long enough that a tab left open
overnight still scrolls the list it started, short enough that `recommendation_item`
holds roughly a day of impressions rather than a week.

Expiry is deliberately **terminal rather than gradual**. Rows are deleted by
`expires_at`, but the client never sees a half-deleted snapshot, because the first
lookup is by `feed_id` and a snapshot with no rows left is unresolvable. `410
feed_expired` tells the client the truth — start again — instead of a 404 it would
have to guess about, or a 200 with a shuffled list, which is the failure mode that
makes people distrust a recommendation feed.
