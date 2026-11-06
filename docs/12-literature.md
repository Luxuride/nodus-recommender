# 12 — Recommendation literature: what we borrowed, what we rejected

Status: draft · Last revised: 2026-11-06

This note records the research the ranking and similarity design leans on, and — more
usefully — the places where the literature says our current approach will fail. It is not a
survey; it is the list of papers that changed a decision, with the decision attached.

## The problem is link recommendation, not rating prediction

Recommending accounts to follow is link prediction on a social graph: we propose an edge, and
success is an edge that gets created. That framing matters because the evaluation literature
for it is different from the movie-ratings literature most tutorials assume.

Barbieri, Bonchi and Manco's survey of link recommendation (*A Survey of Link Recommendation
for Social Networks: Methods, Theoretical Foundations, and Future Research Directions*,
arXiv:1511.01868) is the one to read for the metric question. Two of its observations apply
directly:

- the class balance is extreme — the non-edges outnumber the edges by orders of magnitude, so
  accuracy is meaningless and precision@k on a ranked list is the only honest measure;
- topological scores (common neighbours, preferential attachment) are strong baselines, and
  any learned model has to beat them to justify itself.

That is why `docs/06-ranking.md` sets a baseline of "popular accounts you do not follow"
explicitly: if similarity propagation cannot beat common-neighbour counting on our own graph,
the complexity is not paying for itself and should be removed.

Tang, Hu and Liu's *Social recommendation: a review* (Social Network Analysis and Mining
3:1113–1133, 2013) and Yang, Guo, Liu and Steck's *A survey of collaborative filtering based
social recommender systems* (Computer Communications, 2014) both report the same thing from
the rating-prediction side: social structure helps most where interaction data is thinnest.
That is the justification for the cold-start ladder in `docs/06-ranking.md` — a user with ten
follows should be served by similarity, not by fallback popularity.

## Popularity bias is our largest known risk

Every account-recommendation system that ranks candidates by how many people follow them will
degenerate into a leaderboard, and the literature is unambiguous that the feedback loop makes
it worse: popular accounts are shown, get followed, and are therefore judged more "correct".

- *A Survey on Popularity Bias in Recommender Systems* (arXiv:2308.01118) collects the
  mechanisms and the mitigations. The relevant finding for us: bias compounds through the
  feedback loop, not through a single ranking step, so it must be countered at the level of
  what gets **stored** (`account_affinity`) as well as what gets **ordered**.
- Liu, Cao and Shen's *Popularity Debiasing from Exposure to Interaction in Collaborative
  Filtering* (SIGIR 2023) makes the sharpest point: equalising *exposure* of unpopular items
  does not produce equal *interaction*, so a system that reports exposure-parity as a success
  is measuring its own output rather than its effect.
- Abdollahpouri, Burke and Mobasher's *Managing Popularity Bias in Recommender Systems with
  Personalized Re-ranking* (arXiv:1901.07555) is the practical recipe and is what our MMR pass
  and per-instance cap descend from: re-rank for diversity **per user**, not globally, so a
  user who genuinely wants a niche does not get it diluted by a global quota.
- Cai et al., *Popularity-Aware Alignment and Contrast for Mitigating Popularity Bias* (KDD
  2024) and Chang et al., *Cluster Anchor Regularization* (WSDM 2024) are the current
  learned-model state of the art. We do not use them — they need a trained encoder and a
  training pipeline — but they confirm the direction: the fix is in the representation, not in
  a final multiplying factor.

**Decision impact.** The damping factor in `nodus_core::damping` and the explore slot are
necessary but not sufficient, and the survey explains why: a single multiplier changes the
order of the list without changing what the model is rewarded for learning. The metric that
matters in `docs/01-vision.md` is therefore **follow-through on non-hub accounts**, measured
separately from overall follow-through — otherwise the number will look healthy while the
system drifts toward hubs.

## Offline evaluation will lie to us unless the split respects the timeline

The most expensive lesson in the literature is that a naive hold-out produces numbers that do
not survive contact with production:

- Ji et al., *A Critical Study of Data Leakage in Recommender System Offline Evaluation* (ACM
  TOIS, 2023) shows that a non-global split lets a model learn from interactions that did not
  exist at prediction time, inflating metrics dramatically.
- Le, Liu, Medlar et al., *Don't Get Ahead of Yourself* (RecSys 2025) shows the still-standard
  practice — temporal leave-one-out — leaks, and that the resulting models can predict items
  "from the future".
- *Time to Split* (arXiv:2507.16289, 2025) and *Your 5-Core Train Split May Not Be 5-Core*
  (2025) add the subtle version: filtering and splitting do not commute, so a benchmark can
  claim a property its training split does not have.

**Decision impact.** Our evaluation protocol must be a **global-timeline split**: pick a
cutoff instant, train on everything before it, and evaluate only on follows created after it —
with the candidate set frozen at the cutoff, so a model cannot be scored on accounts it saw
being followed later. The 30-day hold-out already in `docs/05-similarity.md` is the right
shape; what it was missing is the explicit statement that the candidate list is built from
pre-cutoff graph state only. This is cheap to get right once and impossible to correct after
the first promising number.

**No A/B on the live feed without a held-out control group.** The same feedback-loop argument
from the popularity survey means an experiment that shows recommendations to everyone measures
the system's own influence, not its accuracy.

## What we deliberately did not adopt

| Approach | Why not |
| --- | --- |
| Graph neural networks for social recommendation (Sharma et al., *A Survey of GNNs for Social Recommender Systems*, ACM CSUR 2024) | they are the right answer at a scale and data density we do not have; they need a training pipeline, GPU time and a feature store, and they lose the explainability the reason chip depends on |
| Two-tower retrieval with embeddings | the candidate universe is ~1M accounts and our affinity table already holds 500 per user; a learned retriever would replace a `BTreeMap` multiply with a model service and an index to keep warm |
| Inverse propensity weighting in *production* ranking | it is the correct tool for *evaluating* a popularity-biased system and the wrong tool for serving one: it raises variance in exactly the tail we cannot measure yet. Until we have enough traffic for stable IPS estimates, weighting the evaluation is the honest use |
| Multi-armed bandits over the whole feed | the explore slot (5% from ranks 51–200) is a bandit with one arm set and no regret accounting. A real bandit needs reward density we will not have at ten pilot users, and it would make the feed non-reproducible while we are still debugging the ranker |

## Reading order

For someone joining the ranking work: `docs/06-ranking.md` first (what we do), then
arXiv:2308.01118 §3–4 (why popularity bias is structural), then TOIS 2023 on leakage (how not
to fool ourselves), then arXiv:1511.01868 §4 (the baselines we must beat). Everything else is
optimisation on top of those four.
