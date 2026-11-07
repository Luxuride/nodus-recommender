# Design documentation — recommender

These documents are the source of truth for **what** we are building and **why**.
Code follows them; when code and docs disagree, that is a bug in one of them and
the conflict gets resolved here first.

Status: **draft** — written before implementation, expected to change as the
first vertical slice teaches us things. Each document carries its own status line.

This is the **recommender** half of the design. The client has its own set in
[`web/docs/`](../../web/docs/README.md), and the SDK — the wire contract plus the
client state machine — has its own document set in
[`sdk/docs/`](../../sdk/docs/README.md). The client-side contract and state model
are documented there, not here.

The three projects are submodules of one superproject, so they always sit at these
relative paths:

```text
Nodus/            ← superproject: .devcontainer, .gitmodules, README
├── sdk/          nodus-shared, -instance, -sdk   (wire + instance + client state)
├── recommender/      core, store, calculator, feeds · docs/ ← you are here
└── web/          the Leptos client · docs/
```

## Reading order

| Doc | Contents |
| --- | --- |
| [01-vision.md](01-vision.md) | Problem, product shape, users, non-goals, success metrics |
| [02-architecture.md](02-architecture.md) | Repositories, crates, data flow, deployment |
| [03-data-model.md](03-data-model.md) | Postgres schema, indexes, retention, size budget |
| [04-ingestion.md](04-ingestion.md) | Mastodon API access, token model, sync jobs, rate-limit budget |
| [05-similarity.md](05-similarity.md) | Similar-user computation, candidate generation |
| [06-ranking.md](06-ranking.md) | Scoring, ranking, feedback learning, cold start |
| [07-api.md](07-api.md) | HTTP contract served to the client |
| [08-privacy.md](08-privacy.md) | Threat model, consent, data minimization, retention, deletion |
| [09-operations.md](09-operations.md) | Config, migrations, observability, local dev, runbook |
| [10-roadmap.md](10-roadmap.md) | Milestones and what "done" means for each |
| [11-trademark.md](11-trademark.md) | Naming rules that keep the Mastodon mark out of our identity |
| [12-literature.md](12-literature.md) | papers behind the ranking and similarity design, and the ones that say our approach will fail |
| [13-evaluation.md](13-evaluation.md) | the leak-free split, the metrics that are allowed, and `nodus-eval` |
| [decisions.md](decisions.md) | Decision log: choice, alternatives, rationale |

The SDK — wire contract, client state machine, versioning and pinning rules — is
documented in the SDK repository's `docs/`:
<https://github.com/luxuride/nodus-sdk/tree/main/docs>. Read
[03-contract.md](../../sdk/docs/03-contract.md) for the wire contract and
[02-architecture.md](../../sdk/docs/02-architecture.md) for the client state that
the web client renders.

Client documents (why Leptos, scope/parity, data layer, UI, quality) live in
`web/docs/`.

## Vocabulary

Use these words consistently in docs, code and issues.

- **our user** (`app_user`) — a person whose Mastodon token we have seen; either
  per-request only (`request_scoped`) or with a stored copy for background sync
  (`server_sync`). The only entity we hold private signals for.
- **tracked account** (`account`) — any fediverse account we hold metadata for,
  on any instance. Most are not our users.
- **follow edge** — `(follower, followee)`; the backbone of similarity.
- **engagement edge** — favourite, boost, reply, or bookmark, with a weight.
- **candidate** — an account (or post) that survived filtering and is eligible
  for ranking for a specific user.
- **recommendation item** — a scored, ranked candidate that was actually shown,
  persisted so feedback can be attributed to it.
- **impression** — a recommendation item that spent enough time in the viewport
  to be counted as seen.
- **similar user** — another *our user* whose follow/engagement profile is close.
- **SDK** — the crates published from the `sdk` repository: our wire DTOs, the
  typed instance API, and the client's state machine. The only shared code between
  recommender and client.

## Open questions

Tracked here so they are not lost; each blocks or shapes a document.

1. **Scale and hosting target.** Assumed: one self-hosted instance, 100–5,000
   our users, ~1M tracked accounts. Affects the rate-limit budget in
   [04-ingestion.md](04-ingestion.md) and the size estimates in
   [03-data-model.md](03-data-model.md). *Needs an answer before M2.*
2. **Multiple instances per user.** Can one person connect accounts from several
   instances? Assumed: one, in v1; the schema (`app_user` unique instance+id)
   allows more later without migration pain.
3. **Post retention final value.** Now 14 days (`POST_RETENTION_DAYS`) because
   the feed only surfaces posts up to 7 days old. Product input needed before M4.
4. **License.** Assumed AGPL-3.0-or-later (networked service, Mastodon itself is
   AGPL). Confirm before publishing the repositories.
5. **Contract publish target.** Assumed git tags only, no crates.io
   ([03-contract.md](../../sdk/docs/03-contract.md)). Revisit if a third consumer appears.

Answered and folded into the docs (kept for the record): frontend framework →
Leptos (D5); full client vs Feed tab → full client (D6); the Feed tab follows via
the instance directly, not a proxy (D4, revised); telemetry is opt-in per user.

## Conventions

- Markdown, one H1 per file, numbered files, `Status:` line at the top.
- Numbers are contiguous and carry meaning: they are the reading order. When a
document leaves the set (or a new one is inserted), the later files are renamed
with `git mv` so history follows and no number is skipped; the H1, the `Status:`
line and every cross-reference are updated in the same change.
- Cross-link with relative paths; never duplicate a table that lives elsewhere —
  link to it.
- Mermaid diagrams are allowed where a picture removes a paragraph.
- Design changes land as a doc change first, then code. A change to a decision in
  [decisions.md](decisions.md) is a `D`-numbered edit, and superseded decisions
  stay in the log.
