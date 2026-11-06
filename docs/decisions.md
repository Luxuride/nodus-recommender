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