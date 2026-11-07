//! Pure, deterministic recommendation math for the Mastodon recommendation engine.
//!
//! Everything in this crate is a function of its arguments: no I/O, no clock, no
//! randomness, no global state. The database layer feeds in plain values and
//! stores the results, which keeps ranked output reproducible per
//! `(user, day, model_version)` as `docs/06-ranking.md` requires.
//!
//! The formulas and constants mirror `docs/05-similarity.md` (inverse-popularity
//! weights, user–user similarity, candidate generation, popularity damping) and
//! `docs/06-ranking.md` (account ranking, post ranking, feedback labels,
//! diversity). Those documents are the source of truth: when a constant changes
//! there, change it here.

#![forbid(unsafe_code)]

pub mod damping;
pub mod diversity;
pub mod drift;
pub mod evidence;
pub mod idf;
pub mod labels;
pub mod post;
pub mod ranking;
pub mod similarity;
pub mod weighted_set;

pub use damping::{AFFINITY_DAMPING, LIFT_CEIL, LIFT_FLOOR, damp_popularity, popularity_lift};
pub use diversity::{
    Diversifiable, EXPLORE_RATE, INSTANCE_WINDOW, MAX_PER_INSTANCE, MMR_LAMBDA, mmr_select,
};
pub use drift::{
    STABILITY_TAU_DAYS, Stability, TASTE_TAU_DAYS, recency_weight, stability_bonus, weighted_follow,
};
pub use evidence::{
    PENALTY_NORMALISER, base_penalty, confidence, decayed_penalty, dwell_correction,
    normalized_penalty, shrink,
};
pub use idf::{follow_weight, status_weight, target_weight};
pub use labels::{EventKind, label, label_with_dwell};
pub use post::{
    MEDIA_PENALTY, POST_HALF_LIFE_HOURS, POST_MAX_AGE_DAYS, REPLY_PENALTY, SEEN_PENALTY, post_score,
};
pub use ranking::{CandidateSignals, RankingWeights, SUPPORT_SCALE, score};
pub use similarity::{
    CANDIDATE_LIMIT, CO_ENGAGEMENT_WEIGHT, MAX_SIMILAR_USERS, MIN_SIMILARITY, SimilarityBreakdown,
    SimilarityWeights, TWO_HOP_WEIGHT, UserSignals, blend, compare,
};
pub use weighted_set::WeightedSet;
