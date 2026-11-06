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

# database
docker compose -f recommender/compose.yml up -d
docker compose -f recommender/compose.yml exec postgres psql -U mastodon -c '\dx'

# recommender
cd recommender && cargo run --bin nodus-api
cd recommender && cargo run --bin nodus-sync

# web (proxy /api → localhost:3000)
cd web && trunk serve --port 8080
```

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

`readyz` reports whether the required keys are present (never their values).

## Migrations

`recommender/migrations/*.sql`, applied with `sqlx migrate run`. Rules:

- migrations are append-only once merged; a mistake is fixed by a new migration;
- every migration is safe to run on a live database (no long locks: add columns
  nullable, backfill in batches, add constraints `NOT VALID` then validate);
- CI runs `sqlx migrate run` against an empty database and against the last
  production dump's schema, and fails if the two diverge.

## Deployment topology

```text
CDN (static)  ── web/dist, immutable hashed assets, SPA fallback to index.html
API host      ── nodus-api (1..n), /api/v1 only, no static files
Worker host   ── nodus-sync (exactly 1 while the queue is Postgres-based)
Postgres      ── single primary, backups 7 days, no read replicas yet
```

One worker is a deliberate constraint: the job claim uses `FOR UPDATE SKIP
LOCKED`, so more than one is safe, but the per-host rate limiter is in-process.
Scaling to N workers means moving the token bucket into Postgres first (a
`rate_bucket` table with `UPDATE ... RETURNING`), which is documented as the
step, not done ahead of need.

The API is stateless: any instance can serve any request, and the contract
version is part of the image.

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
