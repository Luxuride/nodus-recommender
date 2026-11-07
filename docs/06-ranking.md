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
score(u,c) =  w_sim      · affinity[u,c] · taste_factor(u,c) · popularity_lift(c)
            + w_support  · log1p(support[c]) / log1p(50)
            + w_eng      · engagement_prior(c)
            + w_fresh    · activity_recency(c)          -- exp(-age_days / 30)
            + w_local    · (c.instance_host = u.instance_host)
            + w_lang     · (u.languages ∩ c.languages ≠ ∅)
            + w_stable   · stability_bonus(u,c)          -- exp(-shown_age_days / 7), capped
            − w_fatigue  · attention(u,c, 7d)
            − w_penalty  · penalty(u,c)                  -- decaying, shrunk by evidence, 0..1
```

`penalty` is not a flag. A rejection is stored as a strength — `3` for a `dismiss`,
`5` for a `not_interested` — and read back as `strength · 0.5^(age / half_life)`,
halving every **7 days** for a `dismiss` and **30 days** for a
`not_interested`, then scaled by `n / (n + 3)` where `n` is the impressions this
user has had of that account, and finally divided by the strength of a fully
observed rejection (`5`). So `1.0` is not "was rejected": it is "was rejected and
we have seen this user look at the account enough times to mean it". One impression
gives `0.25`; twenty-five give `0.89`. Nothing runs to expire it, and no account is
excluded forever: the explore channel keeps sampling accounts whose penalty is
decaying, which is what gives later positive evidence a chance to outvote it.

Because of that shrinkage, the *instruction* is honoured by a separate rule rather
than by the score: a fresh `not_interested` (younger than 30 days) is skipped in the
candidate path, while everything older is left to the penalty. Otherwise "not
interested" would either hide an account forever or be outvoted by a good affinity
score the day after the user said no.

`taste_factor` is `1 − w_taste · (1 − exp(−follow_age_days / 180))`: taste decays
with a 180-day constant, and `w_taste` decides how much of that decay survives into
the score. `popularity_lift` is `clamp(follow_rate / expected_rate, 0.25, 4.0)` and
replaces plain follower-count damping as the ordering signal (see
[12-literature.md](12-literature.md): dividing by what *our* users do beats
dividing by how famous an account is).

| Weight | Initial value | Meaning |
| --- | --- | --- |
| `w_sim` | 1.00 | the graph evidence, already popularity-damped in [05](05-similarity.md) |
| `w_support` | 0.35 | how many distinct similar users back the candidate |
| `w_eng` | 0.25 | account-level engagement prior (favourites/boosts/replies on its posts, popularity-normalized) |
| `w_fresh` | 0.20 | recently active accounts are more likely to be worth a follow |
| `w_local` | 0.10 | mild home-instance preference: following locally is cheaper socially and technically |
| `w_lang` | 0.10 | only when the user's language is known and the candidate's posts are in it |
| `w_fatigue` | 0.30 | repeated exposure without attention is a negative signal; the unit is one measured read, not one page load |
| `w_taste` | 0.20 | how much a stale follow weakens the graph evidence behind a candidate |
| `w_stable` | 0.15 | hysteresis: an account shown in the last week keeps a small bonus, so the list does not reshuffle under a reader |
| `w_dismiss` | 2.00 | explicit "not interested" — effectively excludes the account |

Weights are seeded from intuition, then learned (below). Values live in
`model_version.weights`; the active row is read once per request and cached for
60 s.

### Consistency and drift

A recommender that changes its mind every refresh reads as unreliable even when
each individual change is defensible; one that never changes its mind stops
matching the person using it. Two terms hold both halves:

- **Taste decay** (`TASTE_TAU_DAYS = 180`) ages the *evidence*. A follow from
  three years ago is a weaker statement about who you are now than one from last
  month, so the graph score behind a candidate fades with the age of the follows
  that produced it. `recency_weight` clamps at 1.0 for negative ages, because a
  clock skew must never *boost* a signal.
- **Stability hysteresis** (`STABILITY_TAU_DAYS = 7`, capped at `w_stable`) buys
  *continuity* in the visible list: something shown yesterday gets a small bonus,
  so equal-score candidates do not swap places every thirty seconds. It is
  bounded on purpose — hysteresis that can outvote evidence becomes stickiness,
  which is how a feed ends up recommending the same five accounts for a year.

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

### Fatigue counts attention, not page loads

`attention(u, c, 7d)` sums impressions of `(u, c)` over the last seven days, each
weighted by how long it was on screen relative to *that reader's own* median dwell:

```text
attention = Σ over impressions e:
    dwell(e) is null            → 1.0        (not measured: count it as one)
    baseline(u) is null         → 1.0        (fewer than 20 measured samples)
    otherwise                   → min(dwell(e) / baseline(u), 3.0)
```

The baseline is the median, not the mean: a mean is dragged upward by the tab left
open over lunch, which is exactly the behaviour the measurement is meant to
discount. Three impressions a reader lingered on therefore weigh more than five
they skimmed past — under a plain count those five looked worse.

There is no hard frequency cap and no "not shown again for 14 hours" rule. Both were
in an earlier draft and both are wrong for the same reason: they are exclusions that
never decay, so an account the user has genuinely warmed to cannot come back. What
the earlier draft called capping is now the penalty term (§ negatives decay) plus
this fatigue term, both of which are finite and both of which fade.

## Post ranking

The for-you timeline draws from the top ~500 candidate authors plus the user's
own followees' recent boosts. Per post:
```text
post_score = affinity(author)                    -- account score above, normalized
           × recency_decay(created_at, h=8)      -- 0.5^(age_hours / 8)
           × engagement_prior(post)              -- attributed: similar accounts that boosted or replied, weighted by similarity
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

Telemetry (opt-in, [08-privacy.md](08-privacy.md)) gives implicit labels:

| Event | Label | Seed weight |
| --- | --- | --- |
| `follow` | strong positive | +3.0 |
| `boost` / `reply` | positive | +2.0 |
| `click` (profile opened) | mild positive | +1.0 |
| `impression` | exposure, not preference | 0.0 |
| `dismiss` | negative | −2.0 |
| `not_interested` | strong negative | −5.0 |

### Attention is a correction, not a label

Impressions optionally carry `dwell_ms` (separate opt-in; `None` when not
measured or not consented). The client writes the impression when the card leaves
the screen rather than when the page arrives (D36), so the row carries both the fact
and the time — and only one row per item, because the denominator is what every rate
below is divided by. It never becomes a label of its own, because dwell
conflates reading, leaving a tab open and a slow network:

```text
dwell_correction = 0.35 · tanh( ln(dwell / baseline) / 2 )     clamped to ±0.5
label(kind) + dwell_correction
```

`baseline` is **that user's own median dwell**, never a global threshold — a person
who habitually leaves posts open has a high baseline, so only *their* unusually
long dwells score. The ±0.5 bound is half a click, so attention can nudge a label
but never reclassify an action; the client sends nothing unless the tab was visible
and focused and the item was actually rendered for at least 250 ms.

### Negatives decay; nothing is permanent

A rejection is stored as `(strength, recorded_at)` and read back decayed — 7-day
half-life for `dismiss`, 30 for `not_interested` — and scaled by `n / (n + 3)` over
that user's impressions of the account. The design consequence is a requirement on
serving: **the explore channel must keep sampling accounts whose penalty is
decaying**, or the decay is decorative because nothing can ever lift it. Ladder:

| Signal | Magnitude | Half-life | Outvoted by |
| --- | --- | --- | --- |
| fast skip | ±0.5 correction | none (per impression) | anything |
| dismiss | −2.0 | 7 days | ordinary positive evidence |
| not_interested | −5.0 | 30 days | repeated positive evidence |
| follow / boost / reply | +3.0 / +2.0 | 180 days (taste drift) | time only |

### Exploration is a channel, not a tie-break

Five percent of a page is drawn from ranks **51–200**, never from the head: at most one
slot on a twenty-item page, two on forty, five on a hundred. The choice is
deterministic in the feed's seed (`nodus_core::explore::choose`, a partial Fisher–Yates
over a SplitMix64 stream), so reloading page one inside the same snapshot shows the same
list, and two feeds replicas serving the same snapshot agree without talking to each
other. A page shorter than four items gets no slot at all — taking one of three is not
exploration, it is a coin flip with the user's decision.

Without this channel the rest of this section is decoration. A rejection decays only if
something can later produce positive evidence about that account, and a feed that shows
the head of the ranking forever never does; a new account has no affinity, so it lives
in the tail and would never leave it. The explore slot is also how a decaying penalty is
*measured*: the impressions it produces are the `n` in the shrinkage, so an account
rejected once and later engaged with recovers on evidence rather than on a timer.

### Thin evidence moves nothing

Every correction is shrunk by `n / (n + prior)`: `prior = 25` impressions for a
user's dwell baseline, `3` for an account's penalty. Three impressions give 11% of
a correction's strength; two hundred give 89%. Explicit instructions (`follow`,
`dismiss`, `not_interested`) are *not* shrunk — they are commands, not estimates,
and their inertia lives in the half-life. The explore rate scales the same way, so
a brand-new user's feed does not get reshuffled by random probes.

### Popularity is not quality

`favourite_count` is a population count, so it belongs in the **denominator**: it
survives only as `w_status = 1 / ln(e + favourite_count)`, which discounts redundant
evidence (a boost of a viral post says little). It is never a quality signal — a
`log1p(fav + 2·boost + 2·reply)` term ranks what everybody already liked, which is
the behaviour this project exists to replace. `engagement_prior` therefore counts
**attributed** engagement only: boosts and replies by accounts similar to this user.
Zero is the honest value when there is none, and the popularity rung is labelled as
unpersonalised in the UI rather than quietly ranked as if it were good.

**Model.** Logistic regression over the same features used above (plus reason
code, support bucket, follower-count bucket, shared-instance flag, hour-of-day
bucket). Trained offline from `interaction_event` joined to `recommendation_item`
and `account`; ~50k labelled impressions is enough to beat seeded weights.

**Pipeline.** `cargo run -p nodus-tools -- bin fit-ranker` (offline bin
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
