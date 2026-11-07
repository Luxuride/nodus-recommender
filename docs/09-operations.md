# 09 — Operations

Status: revision 1 · Last revised: 2026-11-06

## Local development

Three repositories, one container:

```text
Nodus/
├── .devcontainer/           rust + wasm32 + trunk + docker (shared host daemon)
├── sdk/
├── recommender/                 compose.yml here (Postgres)
└── web/
```

The devcontainer at the root mounts all three, which is why the `path`
dependencies resolve. Opening a single repo alone works for reading, not for
building.

```sh
# first boot (postCreateCommand does this)
rustup target add wasm32-unknown-unknown
trunk --version
tailwindcss --help   # pinned standalone CLI, no Node

# database
docker compose -f recommender/compose.yml up -d
docker compose -f recommender/compose.yml exec postgres psql -U nodus -c '\dx'

# recommender (two processes, one from each terminal)
cd recommender && cargo run --bin nodus-feeds
cd recommender && TOKEN_ENC_KEY=$(openssl rand -base64 32) cargo run --bin nodus-calculator

# web (proxy /api → localhost:3000)
cd web && trunk serve --port 8080
```

### The first run, end to end

This is the path a new user takes, and the order matters because each step is what
makes the next one show something real:

1. **Sign in.** `http://127.0.0.1:8080` shows one field: the instance host. The
   client registers itself on that instance (`POST /api/v1/apps`), sends the
   browser to `/oauth/authorize` with a PKCE challenge, and exchanges the code at
   `/oauth/callback`. Paste-a-token still exists under "advanced" for instances
   whose OAuth is broken. Nothing about the token reaches our server here.
2. **The feed answers immediately, badly.** With no `account_affinity` rows the
   feed is served from the instance-popular rung: accounts ranked by followers,
   never by affinity, with `degraded: "cold_start"` and reasons of
   `popular_fallback`. That is not a bug; it is the ladder working.
3. **Opt in to background sync** (Settings). The token is sealed with
   `TOKEN_ENC_KEY` and one `sync_user` job is queued. `nodus-calculator` claims it,
   walks the account's follows at one request per second per host, writes
   `follow_edge`, and enqueues `recompute_affinity` and `recompute_similarity`.
4. **The feed changes character.** Within a few minutes `account_affinity` has
   rows, the feed stops being popular and starts being justified, reasons switch
   from `popular_fallback` to `followed_by_similar_users`, and
   `degraded` disappears. If it does not, the queue is the place to look:
   `SELECT kind, status, attempts, last_error FROM job ORDER BY id DESC LIMIT 20;`
5. **Follow one of the recommendations.** The client follows on the instance with
   the user's own token (our API never proxies a follow), posts a `follow` event,
   and the recommender deletes that affinity row and queues a fresh `sync_user`.

Without step 3 the stack is still usable — it just keeps serving the cold-start
rung, which is the documented behaviour of `request_scoped` mode in
`04-ingestion.md`.

Tests: `cargo test --workspace` in `contract` and `recommender`; `trunk build
--release` plus the Playwright suite in `web`. Integration tests that need the
database use `sqlx::test` against the compose Postgres, with one database per
test.

## Configuration

Everything through the environment; a typed `Config` struct fails fast at boot
with the offending key named.

| Key | Required | Default | Notes |
| --- | --- | --- | --- |
| `DATABASE_URL` | yes | — | Postgres DSN |
| `LISTEN_ADDR` | no | `0.0.0.0:3000` | API bind address |
| `TOKEN_ENC_KEY` | yes | — | 32-byte key, base64; XChaCha20-Poly1305 |
| `WEB_ORIGINS` | yes | — | comma-separated CORS allow-list |
| `PUBLIC_BASE_URL` | yes | — | absolute links in export/emails-free flows |
| `RUST_LOG` | no | `info,sqlx=warn` | `tracing-subscriber` filter |
| `SYNC_ENABLED` | no | `true` | kill switch for the worker |
| `SYNC_MAX_RPS_PER_HOST` | no | `1.0` | self-imposed ceiling |
| `SYNC_BUDGET_FRACTION` | no | `0.5` | never use more than this share of the reported budget |
| `EXPANSION_MAX_ACCOUNTS` | no | `20000` | P1 crawl cap |
| `BLOCKED_INSTANCES` | no | — | comma-separated, complements the DB flag |
| `MAX_ACCOUNTS_PER_HOST` | no | `5000` | storage guard |
| `POST_RETENTION_DAYS` | no | `14` | status retention |
| `EVENT_RETENTION_DAYS` | no | `90` | telemetry retention |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | no | — | traces, optional |
| `REDIS_URL` | no | — | absent ⇒ in-process caches; refused when the replica count is set above 1 |
| `FEEDS_DB_MAX_CONNECTIONS` | no | `10` | pool for the feeds role, per replica |
| `CALC_DB_MAX_CONNECTIONS` | no | `4` | pool for the calculator role, per replica |
| `CALCULATOR_REPLICAS` | no | `1` | declares how many replicas run; without Redis it is the guard that keeps per-host budgets honest |
| `FEED_CACHE_SECONDS` | no | `60` | Redis TTL for a rendered feed page |
| `VERIFY_CACHE_SECONDS` | no | `600` | token verification TTL |

`readyz` reports whether the required keys are present (never their values).

## Migrations

`recommender/migrations/*.sql`, applied with `sqlx migrate run`. Rules:

- migrations are append-only once merged; a mistake is fixed by a new migration;
- every migration is safe to run on a live database (no long locks: add columns
  nullable, backfill in batches, add constraints `NOT VALID` then validate);
- CI runs `sqlx migrate run` against an empty database and against the last
  production dump's schema, and fails if the two diverge.

## Container image

One image, two roles: `BINARY=nodus-feeds` or `BINARY=nodus-calculator`. The build context
is the **superproject**, because the service depends on the SDK by path:

```sh
docker build -f recommender/Dockerfile -t nodus-recommender .
```

`recommender/Dockerfile.dockerignore` keeps `web/`, `target/` and git metadata out of the
context. Manifests for Kubernetes live in `recommender/deploy/k8s` (kustomize) with a
`deploy/README.md` explaining what is deliberately absent — Redis, Postgres and ingress
are deployment-specific.

## Deployment topology

```text
CDN (static)   ── web/dist, immutable hashed assets, SPA fallback to index.html
feeds          ── nodus-feeds, N replicas, /api/v1 only, no static files
calculator     ── nodus-calculator, M replicas, no user traffic
Redis          ── shared caches, per-host budgets, global locks (degradable)
Postgres       ── single primary, two roles, backups 7 days, no replicas yet
```

**Feeds: N replicas, stateless.** No in-process state that a request depends on:
the token cache and the rendered-page cache are Redis, the feed snapshot is a
Postgres row. HPA on RPS and p95 latency, `minReplicas: 2`, `maxReplicas: 20`,
PodDisruptionBudget `minAvailable: 1`, rolling update with `maxSurge: 1`.
Any replica can serve any request, and the contract version is part of the
image.

**Calculator: M replicas, safe to run in parallel.** The claim uses
`FOR UPDATE SKIP LOCKED`, so replicas take disjoint jobs; a lease deadline
re-queues whatever a dying replica held. Two things must be shared to make M
correct:

- **Per-host budgets live in Redis.** Without it, M replicas each spend the full
  budget and the effective rate is M× the promised one. With `REDIS_URL` absent
  the service refuses to start when `CALCULATOR_REPLICAS > 1`, because the honest
  behaviour is to refuse a configuration it cannot honour.
- **Whole-graph jobs take a lock.** `recompute_similarity`, `recompute_affinity`
  and `fit_model` acquire `lock:recompute` (SET NX with a TTL and owner token,
  renewed while running). One replica runs them; the others keep draining P0–P4,
  which is where the user-visible latency lives. A dead owner's lock expires and
  the work is retried, which is safe because these jobs are idempotent — they
  recompute, they do not accumulate.

Scale it on **queue depth and per-tier job age**, not on CPU: the crawler is
rate-limit bound, so extra replicas mostly add in-flight requests up to the
per-host budget. `minReplicas: 1`, `maxReplicas: 8`.

**`audience_weight` is derived, and stale by one pass.** D24 sizes the refresh
interval (hot/warm/cold) and the scheduler's ordering from
`account.audience_weight`, the number of users whose affinity set contains an
account. Only the affinity recompute maintains it (`store::audience::recompute`,
called at the end of every `recompute_similarity` pass): one statement lifts the
count for accounts that have affinity rows, a second zeroes accounts that dropped
out of every set, and both are guarded so an unchanged pass rewrites nothing.
Treat the column as **eventually consistent by one recompute cycle**: between a
user's affinity rewrite and the next pass, a newly popular account is still on
its old, colder tier and a fallen one is still refreshed too often. The scheduler
must tolerate that — staleness costs at most an extra or a delayed fetch, never a
wrong recommendation — and must never compute the count with `COUNT(*)` in the
scheduling query, which would put a group-by on `account_affinity` on the hot
path.

**Postgres: one primary, two roles.** `nodus_feeds` (read on derived tables,
insert on `recommendation_item`/`interaction_event`, read-write on `app_user`
for consent) and `nodus_calculator` (read-write on graph and derived tables).
`statement_timeout` is 1 s for the feeds role and 30 s for the calculator, and
`application_name` distinguishes them in `pg_stat_activity`, which is the first
thing to look at when the feed slows down and the queue does not.

## CI per repository

| Repo | Jobs |
| --- | --- |
| `contract` | fmt, clippy `-D warnings`, test, `cargo build --target wasm32-unknown-unknown`, compatibility build of `recommender@main` and `web@main` against the working tree, tag on merge to `main` when `CONTRACT_VERSION` changed |
| `recommender` | fmt, clippy, test (with Postgres service), `sqlx migrate run` on empty DB + last dump schema, build image, `cargo deny` |
| `web` | fmt, clippy `--target wasm32`, `trunk build --release`, wasm size budget check, Playwright against the mock instance, `cargo deny` |

Nightly in `recommender` and `web`: build against the **latest contract tag** so pin
drift is discovered by the schedule rather than by a release.

## Observability

Metrics (Prometheus, `/metrics`, internal only):

| Metric | Type | Use |
| --- | --- | --- |
| `api_requests_total{route,status}` | counter | error budget |
| `api_request_duration_seconds{route}` | histogram | p95 latency |
| `api_contract_version_total{contract}` | counter | when `min_contract` can rise |
| `feed_requests_total{kind,degraded}` | counter | cold-start rate |
| `feed_rank_duration_seconds` | histogram | ranking cost |
| `sync_requests_total{host,status}` | counter | rate-limit health |
| `sync_rate_limited_total{host}` | counter | budget pressure |
| `sync_job_age_seconds{tier}` | gauge | queue lag |
| `sync_token_revoked_total` | counter | churn signal |
| `db_pool_in_use` / `db_pool_size` | gauge | saturation |

Traces: one span per HTTP request and per Mastodon call, with instance host and
route template as attributes. No tokens, no handles, no status text.

Alerts (minimum viable): API 5xx > 1% for 10 min; `readyz` failing; worker
heartbeat older than 5 min; job lag P0 > 1 h; `sync_rate_limited_total` growing
for 30 min; disk > 80%.

## Backups and disaster recovery

- Nightly `pg_dump` (custom format) + WAL archiving if the host allows it; 7-day
  retention, encrypted at rest.
- Restore drill quarterly: restore into a scratch database, run `sqlx migrate
  run`, boot the API against it, hit `/readyz` and one feed request.
- Losing the DB is recoverable but expensive: tokens are gone (users re-consent),
  similarity is rebuilt over days. This is documented in the runbook rather than
  papered over with a warm standby nobody tests.

## Runbook: common incidents

| Symptom | First check | Action |
| --- | --- | --- |
| Feed returns `degraded: stale_similarity` | worker heartbeat, job lag | restart worker; if rate-limited, lower `SYNC_MAX_RPS_PER_HOST` and wait |
| Many `401` from one host | that host's status page / `last_error` | mark instance degraded, stop sync for it, surface "sync paused" in UI |
| API 5xx spike after deploy | `api_requests_total` by route, logs | roll back the image (contract is additive, so old client keeps working) |
| Worker stuck claiming jobs | `SELECT count(*) FROM job WHERE locked_at < now() - interval '10 min'` | release stale locks (the claim query does this on start), investigate the failing job's `last_error` |
| Token decryption failures | `TOKEN_ENC_KEY` rotation history | keys are versioned by prefix; support two keys during rotation, re-encrypt lazily |
