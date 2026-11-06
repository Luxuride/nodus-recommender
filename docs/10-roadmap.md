# 10 — Roadmap

Status: revision 1 · Last revised: 2026-11-06

Ordered by risk retired, not by feature count. Each milestone has an exit
criterion that can be checked by running something, and each ends in a tagged
release of the repositories it touches.

## M0 — Documentation (current)

The design above, plus the client doc set in `web/docs/`.

**Exit:** every open question in [README.md](README.md) is either answered or
explicitly deferred; `contract`, `recommender` and `web` repositories exist with
their scaffolds building; `cargo clippy -D warnings` and `trunk build --release`
are green in CI.

## M1 — Contract v0.1 + recommender skeleton

Scope: `nodus-store` migrations for the tables in
[03-data-model.md](03-data-model.md) (minus `status` and `model_version`),
`/healthz`, `/readyz`, `/api/v1/meta`, bearer verification against a real
instance, `PUT /api/v1/sync/token` with AEAD storage, `sync_user` (P0) landing
`follow_edge` rows, and the `job` queue with `SKIP LOCKED`.

**Exit:** pointing a real token at a real instance populates `account` and
`follow_edge` for that user, twice, idempotently; a revoked token stops the job
and is reported by `/api/v1/me`. Contract tag `v0.1.0`.

Risks retired: sqlx + Postgres shape, Mastodon auth reality, token encryption,
job claiming under concurrency.

## M2 — Feed, honestly ranked

Scope: IDF weights, `user_similarity` (single-source co-follow only),
`account_affinity` candidates, hand-tuned scorer, `/api/v1/feed/accounts` with
reasons, `POST /api/v1/events` for impressions/follows, `no_server_sync`
degradation.

**Exit:** 10 real users, one week, feed returns ≥ 20 items with reasons, p95
`feed_rank_duration_seconds` < 150 ms at 1,000 accounts, and follow-through on
the Feed tab beats a "popular accounts you do not follow" baseline in a
side-by-side eval. This is the first milestone that can prove or kill the idea.

Risks retired: whether the similarity signal is worth anything at this scale.

## M3 — Client parity (read + engage)

Scope: `web` — PKCE sign-in, home/local/federated timelines with streaming,
notifications, profiles, thread view, compose (text only), Feed tab wired to
M2's API, IndexedDB cache, optimistic follow/boost/favourite, CSP.

**Exit:** a user can log in, read, post, and follow from our client for a day
without opening the instance's own web UI, at ≤ 900 KB and TTI ≤ 2.5 s on a
throttled connection. Contract tag `v0.2.0`.

Risks retired: client feasibility, streaming, cache invalidation, the actual
browser take on "blazingly fast".

## M4 — Signals beyond follows

Scope: engagement and 2-hop candidates, `refresh_account` and P1 expansion under
the rate limiter, `fetch_statuses` and post recommendations
(`/api/v1/feed/posts`), fatigue and diversity, `not_interested`, offline logistic
refit with `model_version` promotion.

**Exit:** ablation table in `05-similarity.md` filled in with measured recall@20
and novelty on a temporal hold-out; P1 expansion keeps observed 429s under 0.5%
of requests.

## M5 — Daily-driver completeness

Scope: lists, bookmarks, search, filters, moderation, media, custom emoji,
keyboard shortcuts, i18n, follow requests, drafts, PWA.

**Exit:** the parity matrix in `web/docs/01-scope.md` has no M2/M3 gaps; a
5-person pilot uses it as their primary client for two weeks.

## M6 — Operability

Scope: dashboards and alerts from [09-operations.md](09-operations.md), restore
drill executed and written down, deletion job verified end-to-end (export →
delete → purge → `purge_token_total`), contract `min_contract` actually raised
once with metric evidence, `cargo deny` and dependency review in every repo.

**Exit:** an on-call engineer who did not build the system can follow the runbook
through a simulated instance 429 storm and a simulated token-revocation wave.

## Deferred (explicitly not now)

- pgvector embeddings, explanation models, cross-instance popularity graphs.
- Multiple workers (needs `rate_bucket` in Postgres first).
- Server-side rendering, native/desktop shells, admin UI.
- Any write path of ours into a user's account (follows, boosts) — the client
  does those directly.
- Federation of recommendations between instances (interesting, and a privacy
  project in its own right).

## Sequencing constraint worth naming

M2 gates M3: building a fast client for a feed that does not yet rank well is the
expensive way to learn the feed does not rank well. The reverse order (M3 before
M2) only makes sense if the client is the product and recommendations are a
feature; that is not the bet here.
