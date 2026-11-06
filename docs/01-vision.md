# 01 — Vision

Status: draft · Last revised: 2026-11-06

## Problem

Mastodon's discovery problem is social, not algorithmic in the search-engine
sense. A new or lightly-connected account sees:

- a **local timeline** that is whatever one instance's moderators allow,
- a **federated timeline** that is a firehose sorted by recency, dominated by
  the loudest accounts,
- **boosts** from people you already follow, which reinforce the same cluster.

The missing primitive is: *"who else should I follow, given who I already follow
and what people like me engage with?"* Every other social graph solved this
(Who to Follow, people-you-may-know). Mastodon largely left it to luck and
manual profile-hopping.

## Product

A recommendation engine that:

1. learns a user's taste from **whom they follow** and **what they engage with**
   (favourites, boosts, replies, bookmarks),
2. finds **similar users** — people with overlapping taste,
3. propagates their follows and engagements into **candidate accounts** the user
   does not follow yet,
4. ranks candidates and their recent posts,
5. shows the result as a **"Feed" tab** with a one-line reason for each item,
6. learns from what the user does with that feed (click, follow, dismiss).

Items 1–4 are the engine. Item 5 is a small client. Item 6 is the loop that
makes it better than a static heuristic.

## Users and scenarios

**Primary: the lightly-connected user.** Follows 20–200 accounts, mostly one
instance, wants to find the good accounts in adjacent clusters. Signs in once,
gets a ranked follow list with reasons, follows a handful, comes back weekly.

**Secondary: the instance admin.** Wants newcomers on their instance to find
people locally, without curating a list by hand. Installs the service with a
compose file and points users at it.

**Explicitly not a target: the lurker.** A user who follows nobody and engages
with nothing has no signal. We degrade to popularity-based suggestions and say
so ([06-ranking.md](06-ranking.md) §Cold start), but we do not pretend to
personalize them.

## What v1 is

- Account recommendations (ranked, with reasons) **and** a for-you post feed
  built from candidate authors.
- A standalone web app (PWA) that provides the Feed tab and reports feedback.
- One Postgres database holding accounts, the follow/engagement graph,
  similarity, recommendation snapshots and interaction telemetry.
- Ingest, ranking and serving as a Rust workspace, deployed as two processes
  (worker + API) against one database.

## Non-goals (v1)

- **Not a Mastodon fork.** We do not modify Mastodon itself; no patches to its
  web UI, no custom instance image. The Feed tab lives in our client.
- **Not a timeline server.** We do not proxy, cache or re-serve the home
  timeline: that traffic goes browser → instance directly. We are a discovery
  plane, not "all of Mastodon" behind one endpoint.
- **Not a general-purpose ML platform.** A small, explainable, hand-tuned scorer
  first; learned weights later, still small and inspectable.
- **No cross-user exposure of private data.** Favourites are private by default
  on Mastodon; we use them only as anonymized edges and never surface one user's
  activity to another ([08-privacy.md](08-privacy.md)).
- **No DMs, no notification content, no media storage.**

## Success metrics

Engine quality (offline, before launch):
- **Follow-through rate** — share of recommended accounts the user follows
  within 7 days. Target v1: > 8% of ranked accounts, > 25% for the top 5.
- **Precision@10** — share of top-10 suggestions that get any positive
  interaction. Target: > 40%.
- **Novelty** — share of recommendations not already reachable by 1 hop.
  Target: > 60%, otherwise we are just restating the existing graph.

System health:
- **Impressions per recommendation** — if high with no interaction, the ranker
  is wasting the user's attention.
- **Dismiss rate** — should fall release over release.
- **Coverage** — share of our users with a fresh (< 24 h) recommendation set.

Cost:
- DB size per our user per month, and requests per our user per day, both
  budgeted in [03-data-model.md](03-data-model.md) and
  [04-ingestion.md](04-ingestion.md). Quality bought with storage or rate-limit
  spend must be justified against these.

## Principles

1. **Explain every item.** If we cannot state why a candidate is here, it does
   not ship. Reasons are computed, not authored.
2. **Prefer the graph over the model.** Co-follow structure is cheap, robust and
   explainable; reach for learned scoring only when the signal is real.
3. **Spend requests where they buy signal.** Rate limits and DB size are
   budgets, not afterthoughts — see the priority tiers in
   [04-ingestion.md](04-ingestion.md).
4. **The user's data is theirs.** Opt-in telemetry, visible reasons, one-click
   delete, no cross-user leakage.
5. **Degrade honestly.** Few signals → say so in the UI, do not silently fall
   back to "popular accounts" pretence.
