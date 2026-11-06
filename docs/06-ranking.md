# 06 — Ranking

Status: draft · Last revised: 2026-11-06

Similarity gives us candidates with a graph score. Ranking decides what the user
actually sees, in what order, and why. It is a small linear model on purpose:
every term is inspectable, every weight is a row in the database, and a bad
release is a pointer flip rather than a retrain.

Two surfaces rank differently:

- **Accounts** (who to follow) — graph score dominates.
- **Posts** (for-you timeline) — author affinity dominates, recency decays fast.

## Account ranking

```text
score(u,c) =  w_sim      · normalize(affinity[u,c], 0, 1)
            + w_support  · log1p(support[c]) / log1p(50)
            + w_eng      · engagement_prior(c)
            + w_fresh    · activity_recency(c)          -- exp(-age_days / 30)
            + w_local    · (c.instance_host = u.instance_host)
            + w_lang     · (u.languages ∩ c.languages ≠ ∅)
            − w_fatigue  · seen_count(u,c, 7d)
            − w_dismiss  · dismissed(u,c)
```

| Weight | Initial value | Meaning |
| --- | --- | --- |
| `w_sim` | 1.00 | the graph evidence, already popularity-damped in [05](05-similarity.md) |
| `w_support` | 0.35 | how many distinct similar users back the candidate |
| `w_eng` | 0.25 | account-level engagement prior (favourites/boosts/replies on its posts, popularity-normalized) |
| `w_fresh` | 0.20 | recently active accounts are more likely to be worth a follow |
| `w_local` | 0.10 | mild home-instance preference: following locally is cheaper socially and technically |
| `w_lang` | 0.10 | only when the user's language is known and the candidate's posts are in it |
| `w_fatigue` | 0.30 | repeated exposure without interaction is a negative signal |
| `w_dismiss` | 2.00 | explicit "not interested" — effectively excludes the account |

Weights are seeded from intuition, then learned (below). Values live in
`model_version.weights`; the active row is read once per request and cached for
60 s.

### Diversity

Ranking alone converges on the same dense cluster. Two constraints, applied in
order:

1. **Per-instance cap** — at most 2 accounts from the same `instance_host` in
   any 10 consecutive items.
2. **MMR re-rank** — `λ = 0.7`:
   `mmr = λ·score − (1−λ)·max_{s ∈ selected} jaccard(F(c), F(s))`
   so a candidate that would duplicate the taste of an already-selected account
   is pushed down. `F(c)` is the candidate's followee set, already loaded for
   scoring.

### Exploration

5% of slots (`EXPLORE_RATE`) are filled from ranks 51–200 of the affinity list
rather than the top, chosen deterministically per user-day. Without this the
ranker can only confirm what it already believes, and a user's taste never
drifts. Exploration items carry the same reason format; no "because we felt like
it" label.

### Fatigue and frequency capping

`seen_count` counts impressions of `(u, c)` in the last 7 days from
`interaction_event`. An account with 3 impressions and no interaction is dropped
from the candidate pool for 14 days; an account with an impression in the last
24 h is not shown again unless the user followed it (then it disappears anyway).
`dismissed` is permanent per user.

## Post ranking

The for-you timeline draws from the top ~500 candidate authors plus the user's
own followees' recent boosts. Per post:
```text
post_score = affinity(author)                    -- account score above, normalized
           × recency_decay(created_at, h=8)      -- 0.5^(age_hours / 8)
           × engagement_prior(post)              -- log1p(fav + 2·boost + 2·reply), popularity-damped
           × reply_penalty                       -- 0.5 if it replies to a non-followee
           × media_penalty                       -- 0.9 if it has no text and the client cannot preload media
           × (1 − 0.15·min(seen_count, 3))       -- do not repeat a post
```

Hard filters: non-public visibility, author blocked/muted, language mismatch when
known, posts older than 7 days (`POST_MAX_AGE_DAYS`), boosts of boosts (collapse
chains to one level).

**Why an 8-hour half-life.** A discovery feed is competing with the home timeline,
which already sorts by recency. If our posts are a week old, the feed feels
broken; if we only show the last hour, we only show our users' own cluster. Eight
hours plus the author-affinity term keeps it fresh without collapsing to
"everything from the last hour".

## Learning from feedback

Telemetry (opt-in, [09-privacy.md](09-privacy.md)) gives implicit labels:

| Event | Label | Seed weight |
| --- | --- | --- |
| `follow` | strong positive | +3.0 |
| `boost` / `reply` | positive | +2.0 |
| `click` (profile opened) | mild positive | +1.0 |
| `impression` | exposure, not preference | 0.0 |
| `dismiss` | negative | −2.0 |
| `not_interested` | strong negative | −5.0 |

**Model.** Logistic regression over the same features used above (plus reason
code, support bucket, follower-count bucket, shared-instance flag, hour-of-day
bucket). Trained offline from `interaction_event` joined to `recommendation_item`
and `account`; ~50k labelled impressions is enough to beat seeded weights.

**Pipeline.** `cargo run -p mastodon-reccom-tools -- bin fit-ranker` (offline bin
reading Postgres, writing `model_version` rows) → validation on a time-based
split → promote by flipping `active`. **No online learning**: the feed never
trains on the request path, so a bad batch cannot corrupt scoring instantly, and
every model is reproducible from a snapshot.

**Guardrails before promotion.** Candidate model must beat the active one on
precision@10 and on dismissal rate in backtest, and must not reduce novelty below
the floor in [01-vision.md](01-vision.md). A model that wins offline and loses
novelty is overfitting to the existing neighbourhood.

## Cold start

An honest ladder, because a bad first impression is expensive:

| Signal available | Strategy | UI label |
| --- | --- | --- |
| ≥ 50 follows **and** engagement history | full model | normal reasons |
| 10–49 follows | similarity + 2-hop only, no learned weights | "people followed by people you follow" |
| 1–9 follows | 2-hop graph + instance-popular accounts | "popular on your instance" |
| 0 follows | onboarding picker: interest categories → curated accounts | "pick a few accounts to get started" |

The onboarding picker (M5) keeps new users out of the ranker entirely until they
have given it something to work with. Until then the PWA shows the ladder state
explicitly rather than pretending personalization exists.

## Guardrails that are not negotiable

- Never recommend an account the user blocks, mutes, dismissed, or that is
  deleted/suspended.
- Never recommend an account from a blocked instance, or one that has moved.
- Never show a post that is not public/unlisted, or that the author deleted —
  deletions propagate at the next status refresh at the latest.
- Every item carries a machine-generated `reason`; no item ships without one.
- Ranked results are deterministic per `(user, day, model_version)` — required
  for debugging and for the offline evaluation harness.
