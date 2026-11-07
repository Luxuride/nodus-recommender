# 08 — Privacy and threat model

Status: revision 1 · Last revised: 2026-11-06

The product is social discovery; the data is other people's public activity and
the user's own follows. The design goal is: **hold the minimum that makes
recommendations possible, prove that, and make deletion real.**

## What we hold

| Data | Why | Encrypted | Retention |
| --- | --- | --- | --- |
| `app_user` row (instance, account id, scopes) | identity mapping | no (not personal content) | until deletion |
| `access_token_enc` | background sync | AEAD, key outside the DB | until consent withdrawn |
| `follow_edge` (user → account) | similarity input | no | until deletion |
| `engagement_edge` (user → status) | similarity input | no | until deletion |
| `account` metadata | candidate display | no | 30 days after last fetch, then pruned |
| `status` text (public/unlisted only) | post recommendations | no | 14 days |
| `interaction_event` | ranking feedback | no | 90 days |
| `job` rows | scheduling | no (ids only) | 7 days after completion |
| Server logs | operations | no | 30 days, no tokens, no request bodies |

Deliberately **not** collected: DMs, follower-only posts, media, IP addresses
beyond the load balancer's ephemeral handling, user agents (except a coarse hash
for the revoked-token UX), any third-party analytics, any advertising identifier.

## Trust boundaries

```text
browser ──(1)──► instance          user's own token; we are not in the path
browser ──(2)──► our API           bearer token over TLS, CORS allow-list
our worker ─(3)─► instance         stored token, read scopes only
our API ───(4)─► instance          verify_credentials only
```

(1) is the bulk of traffic and is invisible to us — the strongest privacy
property the architecture has, and the reason not to build a proxy.

(2) means the user's Mastodon token appears in our request logs' `Authorization`
header if we are careless. Logging middleware must strip it before any sink; a
test asserts that no log line contains `authorization`.

(3) is why consent exists: without it, there is no worker path at all.

## The client token problem

The client must hold a Mastodon token in the browser to talk to the instance
directly. That is inherent to being a client, and it is what Elk and Phanpy do.
Consequences and mitigations:

| Risk | Mitigation |
| --- | --- |
| XSS exfiltrates the token | `script-src 'self'` CSP with no `unsafe-inline`/`unsafe-eval`; no third-party scripts, fonts or CDNs; sanitized instance HTML; no `innerHTML` of untrusted input; no `wasm-bindgen` closures over raw strings into `eval`-like paths |
| Malicious dependency | dependency review gate, `cargo deny` in CI (licenses, advisories), no build scripts from unknown crates |
| Token in URL/history/referrer | token never in a query string except Mastodon's own streaming endpoint (which requires it); `Referrer-Policy: no-referrer` |
| Shared device | explicit "sign out" clears IndexedDB and calls `DELETE /api/v1/me` which revokes at the instance |
| Stale token after compromise | Mastodon tokens cannot be scoped down after issuance; the only remedy is revocation, so the sign-out path is prominent and documented |

We accept this risk explicitly rather than pretend a server-side proxy removes
it: an XSS in a proxy-based design steals the session cookie instead, and a
session-compromise is strictly worse (it can act as the user everywhere,
including in our recommendation engine).

### Attention, and what it is not

When a user opts in to attention measurement, the client reports how long a
recommended card stayed on screen, rounded to the millisecond and capped at 120
seconds. It is one integer per impression, on the same row as the impression itself
(D36) — no pointer positions, no scroll traces, no per-pixel heat map, and no
measurement at all for users who decline. The aggregated value never leaves the
scoring path: it adjusts a label by at most ±0.5 and is discarded by the retention
job with the rest of `interaction_event` after 90 days.

## Consent model

Two independent switches, both off-by-default in the sense that each requires an
explicit action:

| Switch | Endpoint | Effect when off |
| --- | --- | --- |
| Background sync | `PUT /api/v1/sync/token` | no server-side token, no worker crawling for this user; feed ranks on demand from the current request's token |
| Telemetry | `PATCH /api/v1/me/settings` | events accepted and discarded (204); feed still works, personalization uses follow/engagement signals only |

The consent screen states, in plain language: what is stored, where, for how
long, and the single-sentence version of why (recommendations need a follow
graph, which needs to be read while you are not looking at the app).

`DELETE /api/v1/sync/token` is one click and takes effect immediately: token
wiped, `user_similarity` and `account_affinity` rows for that user deleted,
queued jobs for that user cancelled. The feed does not break — it degrades to
`no_server_sync` and says so.

## Deletion and export

- `GET /api/v1/me/export` returns everything we hold about the caller as JSON,
  including derived rows (similarity inputs, affinity), not just raw events.
- `DELETE /api/v1/me`: stop sync, revoke the token at the instance, delete
  telemetry and affinity, soft-delete the user, purge everything after 30 days
  (a job, not a promise).
- Derived aggregates that cannot be attributed (global IDF weights, model
  coefficients) are not personal data and survive deletion; there is no way to
  reconstruct one user from them.

## Third parties

- Instances: we call their public API. Their privacy policy governs their side;
  we do not federate with accounts we cannot read.
- No payment processor, no email provider (no email is collected at all), no
  error-tracking SaaS. Errors go to our own logs.
- Blocked instances (`instance.blocked`) are excluded from crawl and from
  candidate display, which doubles as a moderation lever for CSAM/abuse-heavy
  domains.

## Operational hygiene

- Secrets (`TOKEN_ENC_KEY`, DB URL) from the environment, never in the repo, never
  in logs; `readyz` reports presence, not values.
- Metrics avoid high cardinality: no account handles, no tokens, no full URLs —
  route templates and instance host at most.
- Backups are encrypted at rest and contain the same data as the DB, so the
  deletion job must also be able to rewrite backups' retention window: backups
  are kept 7 days, shorter than the 30-day soft delete, which makes the guarantee
  true in practice.
- A quarterly review re-reads this table against the code; drift here is a bug.
