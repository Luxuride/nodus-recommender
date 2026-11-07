# 13 — Evaluation

Status: draft · Last revised: 2026-11-07 (decisions D0/D26)

A recommendation engine that is not measured is a random number generator with good
copy. This document defines the one protocol the project accepts, the numbers it
reports, and where the code lives.

## The protocol: one instant, everything on either side of it

`follow_edge.first_seen_at` is the timestamp the crawler observed a follow. Pick a
cutoff `T`:

| Slice | Rows | Used for |
| --- | --- | --- |
| Training | `first_seen_at < T` | building signal sets, similarity, candidates |
| Truth | `first_seen_at >= T` | the answer key; never read by the ranking |

Then: rank from the training graph as if `T` were now, and count how many of the
truth accounts appear in the top `k`.

**Why not a random hold-out.** A random split leaks the future into the past: the
candidate set already knows which accounts got popular *after* `T`, so hubs look
predictable and recall looks fine while the model has learned nothing. The leak is
structural, not statistical, and averaging over more users does not remove it —
this is the failure the RecSys literature keeps re-reporting (`Don't Get Ahead of
Yourself`, RecSys 2025; `Time to Split`, arXiv:2507.16289).

**Why a global cutoff and not per-user windows.** A per-user window would compare
different instants across users, so one user's "next 30 days" overlaps another's
training data. One instant keeps every user's answer key from the same world.

## What is reported

| Metric | Meaning | Where it is misleading |
| --- | --- | --- |
| `precision@k` | share of the top `k` that the user did follow | a page of hubs scores well on a graph where everyone follows hubs |
| `recall@k` | share of the user's new follows that were in the top `k` | punishes users with very broad taste |
| bucket recall (`<1k`, `1k–10k`, `≥10k`) | recall split by the *recommended* account's follower count | — it is the metric that stops a hub from standing in for the result |
| popularity baseline | the same metrics for "most-followed accounts you do not follow" | the baseline to beat; if the taste model cannot, the extra machinery is decoration |

The report prints the baseline next to the model on purpose, and ends with a blunt
verdict line rather than a table a reader can cherry-pick.

**What is deliberately not reported.** Precision over the *whole* candidate space,
AUC, and any single scalar. All three reward ranking hubs, and none of them says
whether a user found somebody worth following.

## Running it

```bash
cd recommender
cargo run -p nodus-eval -- --database-url "$DATABASE_URL" --cutoff-hours 24 --k 10

# Planted graph: does the harness itself work?
cargo run -p nodus-eval -- --seed-users 30 --seed-accounts 240 --seed-clusters 6
```

Synthetic mode exists to validate the harness, not the product: users are drawn from
taste clusters, most follows stay inside the cluster, and one fifth of the edges are
stamped after the cutoff. On that graph the taste ranking **must** beat popularity on
small and medium accounts; if it does not, the bug is in the harness.

Measured example (30 users, 240 accounts, 6 clusters, `k = 10`):

```text
ranking         precision     recall
taste               0.097      0.254
popularity          0.013      0.035

bucket             taste       pop     truth
<1k followers      0.333     0.000        27
1k-10k             0.263     0.000        76
>=10k              0.000     0.364        11
```

The hub row is the interesting one: popularity wins there and taste scores zero,
because the planted graph gives hubs no taste structure to discover. That is the
honest reading — on a real instance the hub row is where the two agree, and the
small-account rows are where the engine earns its place.

## Code

| Path | Responsibility |
| --- | --- |
| `crates/eval/src/split.rs` | cutoff-bounded graph load; `scoreable()` skips users with too little history or nothing to find |
| `crates/eval/src/model.rs` | a second implementation of the candidate path over the in-memory graph, reusing `nodus-core`'s weights and damping |
| `crates/eval/src/metrics.rs` | precision/recall, follower buckets, zero-safe division |
| `crates/eval/src/synthetic.rs` | the planted graph and its LCG (reproducible failures) |

The duplication is deliberate. An evaluator that called the production ranking would
answer "what did the product decide"; this one answers "would the same arithmetic,
given only the past, have found what happened next".

## What is still unmeasured

- **Follow-through in the product.** The offline protocol measures *finding*; the
  live metric (`POST /api/v1/events`, `kind = follow`) measures *persuasion*. They
  disagree, and only the second one pays.
- **Post recommendations.** `crates/eval` ranks accounts only; posts need the same
  split over `status` and `engagement_edge`.
- **The reference-profile variant** (D31) — the reason `crates/eval` exists is that
  this comparison cannot be argued, only measured.
