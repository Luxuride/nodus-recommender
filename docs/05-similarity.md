# 05 — Similarity and candidate generation

Status: draft · Last revised: 2026-11-06

The engine has two jobs: find **similar users** (our users whose taste overlaps
the target's) and turn their follows and engagements into **candidate accounts**
the target does not follow yet. Both are deliberately simple, explainable and
computable in one nightly pass.

## Signals

Every signal is a set overlap, so the whole thing is set math over ids.

| Signal | Source | Rationale | Weight |
| --- | --- | --- | --- |
| **Co-follow** | `follow_edge` where `source = 1` (own-user sync) | two people following the same 20 accounts have related taste; the strongest cheap signal | 0.60 |
| **Co-favourite** | `engagement_edge` kind `favourite`/`bookmark` | favouriting the *same post* is a much rarer, much stronger event than co-following | 0.25 |
| **Co-reply** | `engagement_edge` kind `reply`, grouped by `target_account_id` | arguing with the same people implies a shared community | 0.10 |
| **Reciprocity** | mutual `follow_edge` | weak tie-strength hint | 0.05 |

Mega-accounts would otherwise dominate every overlap (everyone follows
`mastodon.social/@Mastodon`), so all overlaps are **inverse-popularity weighted**:

```text
w_follow(i)  = 1 / ln(e + followers_count(i))        -- an account followed by 1M people says little
w_status(s)  = 1 / ln(e + favourite_count(s))        -- a post with 10k favourites says little
w_target(a)  = 1 / ln(e + distinct_engagers(a))      -- so does a person replied to by everyone
```

`followers_count` is the value from the last metadata refresh; stale by up to 7
days, which is irrelevant at this scale.

## User–user similarity

For our users `u` and `v`, with followe sets `F(u)`, `F(v)`:

```text
co_follow(u,v)  = Σ_{i ∈ F(u) ∩ F(v)} w_follow(i) / sqrt( Σ_{i∈F(u)} w_follow(i) · Σ_{j∈F(v)} w_follow(j) )
co_fav(u,v)     = Σ_{s ∈ L(u) ∩ L(v)} w_status(s)   / sqrt( Σ_{s∈L(u)} w_status(s) · Σ_{t∈L(v)} w_status(t) )
co_reply(u,v)   = Σ_{a ∈ R(u) ∩ R(v)} w_target(a)   / sqrt( Σ_{a∈R(u)} w_target(a) · Σ_{b∈R(v)} w_target(b) )
reciprocity(u,v)= 1 if u and v follow each other else 0

similarity(u,v) = ( Σ_c w_c·component_c ) / ( Σ_c w_c over measurable components )

w_co_follow = 0.55   w_co_boost = 0.15   w_co_fav = 0.10   w_co_reply = 0.10   w_reciprocity = 0.05
```

This is cosine similarity over IDF-weighted sets, so each component is bounded to
`[0,1]`.

**Renormalisation, and why it is not optional.** Not every component is measurable
for every kind of neighbour, and the two kinds differ:

| Component | Our own users | Non-user "reference" neighbours |
| --- | --- | --- |
| `co_follow` | yes | yes |
| `co_boost` | yes | yes (a boost is a published status) |
| `co_fav` | yes, if the user opted in to sync | **never** — `GET /api/v1/favourites` only answers for the caller |
| `co_reply` | yes | yes, when their replies are public |
| `reciprocity` | yes | no |

A reference's maximum reachable score is therefore ~0.90 while a user's is 1.0, so
dividing by the *full* weight sum would systematically under-score references and
the top-50 cut would prefer user neighbours for a measurement artefact. The
denominator is the weight sum of the components that were measurable for that pair.
The pruning threshold applies to the renormalised value.

`co_boost` exists for the same reason: an endorsement published as a status is
crawlable for anybody, so it restores some engagement signal exactly where
favourites are unavailable. It also keeps `co_fav` from carrying a quarter of the
weight on data that only opted-in users provide.

**Pruning.** Store `similarity ≥ 0.05`, keep the **top 50** per user.
`signals` jsonb records the raw overlap counts and a few sample account ids, which
is what feeds "because you both follow @x".

**Cost.** With 1,000 users, the naive pairwise pass is 500k pairs. Each pair's
co-follow is an intersection over ~200 ids: microseconds. A nightly job that
loads all `source = 1` edges into memory, inverts them (followee → users), and
accumulates pair scores finishes in seconds. No SQL join gymnastics, no
`pgvector` needed. Scale-out path: bucket users by shared followees and only
compare pairs with ≥ 2 shared items.

**When similarity is not available.** A user with fewer than 20 follows has too
little structure to place; they get the fallback ladder in
[06-ranking.md](06-ranking.md) §Cold start, labelled honestly in the UI.

## Candidate generation

Three sources, combined and deduplicated. For target user `u`:

### 1. Similar-user propagation (primary)

```text
for v in top50_similar(u):
    for c in F(v) \ F(u):
        score[c] += similarity(u,v) · w_follow(c)
        support[c] += 1                     -- how many similar users follow c
```

`support` is what the UI shows ("followed by 4 people you follow") and it is a
better explanation than the raw score.

### 2. Two-hop graph path (broadens coverage)

Accounts followed by people `u` follows — the classic "follows of followees":

```text
for i in F(u):
    k = 0.6  if u has engaged with i      -- favourited, boosted or replied
    k = 0.25 otherwise
    for c in F(i) \ F(u):
        score[c] += k · w_follow(i) · w_follow(c)
        support[c] += 1
```

This is computed from the follow edges alone (`store::two_hop`), needs no similarity
and works from the first sync, which is why it is the backbone for users with few
similar peers. It is also noisier, hence the lower weight and the popularity damping
below.

The damping is not constant. A path through someone the user follows is weak evidence:
everyone they follow leads somewhere, and following an account is a cheap act. But if
the user has *interacted* with that account — favourited, boosted or replied to it —
then what that account follows says something about their taste, and the path is promoted
to `0.6`. That is the adaptive part of the design: engaging with a second-hop account
turns its neighbourhood into first-class candidates, without ever paying to crawl two
hops out of everyone (D35).

### 3. Co-engagement (ties the graph to activity)

Accounts whose posts were favourited, boosted or replied to by ≥ 2 of `u`'s
followees *or* similar users:

```text
score[c] += 0.35 · Σ_{a ∈ engagers(c) ∩ (F(u) ∪ similar(u))} w_engager(a)
```

Favourites of our users are private data, so `engagers(c)` only ever contains
*our users* — a user's own engagement is never revealed to anyone else, it only
feeds aggregate counts ([08-privacy.md](08-privacy.md)).

### Popularity damping

Any candidate that looks like a celebrity gets its score compressed, otherwise
every feed is the same ten famous accounts:

```text
final[c] = score[c] / (1 + ln(1 + followers_count(c)))^0.5
```

The exponent is a tuning knob (`AFFINITY_DAMPING`, default `0.5`); `0` disables
it, which is useful for A/B testing rather than production.

## Filters

Applied before `account_affinity` is written:

- `c == u`, or `u` already follows `c` (including follow edges not yet synced —
  re-check against live follows at request time)
- `c` is blocked or muted by `u` (requires `read:blocks` / `read:mutes`)
- `c.bot` and `u` follows < 2 bots
- `c.last_status_at` older than 90 days
- `c.followers_count = 0`
- `c.instance_host` on the blocklist, or `c.moved_to_account_id` is set
- `c` has been dismissed or marked not-interested by `u` (permanent per user)
- more than `MAX_CANDIDATES_PER_HOST` (50) from one instance in the stored set

## Output

`account_affinity` holds the **top 500** candidates per user with `score`,
`reason_code`, `support`. That cap is what keeps the table at ~30 MB for 1,000
users and makes the feed request a cheap indexed read.

Recomputation:

| Event | Action |
| --- | --- |
| New follow synced (P0) | recompute that user's similarity and candidates immediately (cheap, ~1 user) |
| Nightly (03:00 UTC) | full pass for all users, prune below threshold |
| New tracked accounts | nothing until the next pass; the 2-hop source covers the gap |

## Evaluation before shipping

Temporal hold-out, no telemetry needed:

1. Take our users' follow edges with their `first_seen_at`.
2. Hide every follow observed in the last 30 days.
3. Rebuild similarity and candidates from the remaining graph.
4. Measure `recall@k` and `precision@10` against the hidden follows, and
   `novelty` = share of hits that are not 1-hop reachable.

This detects the two classic failures early: a model that only redisplays the
user's existing neighbourhood (low novelty) and one that recommends dead
accounts (precision collapse). Both are cheap to catch offline; neither is cheap
to fix after launch.
