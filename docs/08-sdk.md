# 08 — The SDK project

Status: draft · Last revised: 2026-11-06 (decisions D10, D13, D14)

`sdk` is a repository, not a service. It is both the **contract** between the
backend and the client and the **source of truth for client state**. Three crates,
and nothing else:

| Crate | Purpose |
| --- | --- |
| `mastodon-reccom-shared` | our wire DTOs (`AccountFeed`, `EventBatch`, `Meta`, error envelope), `CONTRACT_VERSION` |
| `mastodon-reccom-mastodon` | Mastodon domain model (`Status`, `Account`, …) and `InstanceApi`, the instance surface transports implement |
| `mastodon-reccom-sdk` | the client state machine: `AppState`, `Command`, `Sdk<I, D, P, C>` and its four ports |

Anything that would make one consumer's code the other's problem does not belong
here. `core` (ranking) stays backend-only precisely because the client must not
recompute scores; the client renders what it is given.

## Why the contract and the client state live together

They are one question asked at three layers:

- `shared`: what *our* API returns to clients;
- `mastodon`: what *an instance* returns to both the sync worker and the browser;
- `sdk`: what the client *does* with both — session, timelines, feed, streaming,
  cache policy.

The last one is the reason the repository is not a passive "contract": if state
lived in the client crate, the browser would own the only copy of pagination,
cache-expiry and optimistic-update rules, and the sync worker would grow a second
copy. Splitting state out is what lets the web client stay a renderer.

It also keeps the SDK honest: it must compile for `wasm32` *and* `x86_64` with no
framework dependency, so anything web-only cannot sneak in.

## Repository layout and pinning

The three projects are submodules of one superproject:

```text
MastodonReccom/            ← superproject: .devcontainer, README, .gitmodules
├── sdk/      → mastodon-reccom-sdk       (submodule)
├── backend/  → mastodon-reccom-backend   (submodule)
└── web/      → mastodon-reccom-web       (submodule)
```

* The superproject pins each project by **gitlink** (an exact commit), so one
  `MastodonReccom` commit describes a known-good combination of all three.
* Consumers depend on the SDK by **path**, not by `git` URL:

  ```toml
  mastodon-reccom-shared = { path = "../sdk/crates/shared", version = "0.1.0" }
  mastodon-reccom-sdk    = { path = "../sdk/crates/sdk",    version = "0.1.0" }
  ```

  Paths work because the submodules always sit at fixed relative positions. The
  alternative (a `git = "https://…"` dependency) was rejected: it duplicates the
  sources into `~/.cargo/git`, forces a push before a cross-project change can even
  be compiled, and leaves two sources of pinning (gitlink *and* the URL in
  `Cargo.toml`) that can disagree.
* `Cargo.lock` is committed in `backend` and `web`.
* Update procedure: commit in `sdk`, then in the superproject
  `git submodule update --remote sdk` and commit the gitlink. The client picks the
  change up on its next build; no `cargo update` dance, no tag to re-point.

Tags still exist in the SDK repository, because they mark *what* a contract version
means over time and make bisecting an incident straightforward. They are a label,
not the pinning mechanism.

## Versioning

The contract has its own version, independent of `backend` and `web` releases:

```text
0.4.0   ← major.minor.patch
│ │ │
│ │ └── patch: docs, internal refactor, no wire change (consumers need not bump)
│ └──── minor: additive wire change (new optional field, new endpoint, new feature flag)
└────── major: breaking wire change (removed/renamed field, changed semantics)
```

## Rules inside a minor version

1. Additive only: new fields are `Option<T>` or `#[serde(default)]`; new endpoints
   appear behind a `Meta::features` flag.
2. Nothing is removed or renamed inside a minor — deprecate in the docs, remove in
   the next major.
3. `CONTRACT_VERSION` in `mastodon-reccom-shared` is the single source; it is bumped
   in the same commit as the wire change.
4. `min_contract` (served by `GET /api/v1/meta`) only rises with a major version, so
   an old client can still reach a server that has moved on.
5. Tags are annotated, and the changelog entry says which consumers must bump.

## Coexistence

| Situation | Behaviour |
| --- | --- |
| new server, old client | server sends extra fields; client ignores unknown ones (`serde` default behaviour) |
| old server, new client | client compares `meta.contract`/`min_contract` at boot and gates features on `meta.features` |
| server too new for the client (`min_contract` > client's contract) | `426 client_too_old`; the client shows an update screen and keeps timelines working (instance traffic is independent) |
| SDK changed, consumer not rebuilt | impossible: path dependency, so the consumer either builds the new SDK or does not build |

## The client-side boundary

What the SDK owns, and therefore what the web client must not re-implement:

* session lifecycle (sign-in, sign-out, cache clear);
* timeline paging and cache policy (soft TTL, page caps);
* recommendation feed state, local dismissals, degradation flags;
* streaming socket state and reconnect bookkeeping;
* optimistic updates and their rollback.

What it explicitly does not do:

* ranking, scoring or candidate generation (backend);
* rendering, sanitisation, routing, keyboard handling (web);
* token custody: the token is handed to a transport, never stored in `AppState`.

## Compatibility CI

Every SDK PR runs:

1. `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`;
2. `cargo check --workspace --target wasm32-unknown-unknown` — the SDK is compiled
   for the browser too, so a wasm-only build failure is a broken contract;
3. a checkout of `backend@main` and `web@main` built against the working tree.

The third step is the one that matters: it is the only place where "this change
still compiles for both consumers" is actually proven.

## Failure modes

| Failure | Symptom | Handling |
| --- | --- | --- |
| consumer updated, SDK not | build error (missing field/type) | expected: path dependency means the failure is loud and immediate |
| SDK minor bump with a non-additive change | old client fails to deserialize | review rule 1; CI builds the previous consumer tag against the new SDK |
| `CONTRACT_VERSION` not bumped | clients cannot detect the mismatch | bump is part of the review checklist for `shared/` diffs |
| `min_contract` raised too early | existing clients get `426` | raise only when `api_contract_version_total` shows no traffic from the old version |

## Relationship to the Mastodon API

`mastodon-reccom-mastodon` models the parts of the instance API the client and the
worker both use. It is not a complete Mastodon client: missing endpoints are added
when a consumer needs one, in the same additive spirit (a new trait method with a
default-free requirement is a breaking change for 1.0, which is why the SDK stays
below 1.0 until the M3 parity matrix closes).
