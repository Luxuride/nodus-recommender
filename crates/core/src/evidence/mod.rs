//! How much weight one observation is allowed to carry.
//!
//! Three ideas live here, and they exist because a recommender is an easy machine
//! to make unstable:
//!
//! * **Bounded corrections.** A dwell time is evidence about attention, not a
//!   verdict. It adjusts a label by at most [`DWELL_MAX_CORRECTION`], so it can
//!   never turn a follow into a dismissal.
//! * **Decaying penalties.** A rejection is evidence too, and evidence about a
//!   person gets stale. Penalties halve on a documented schedule instead of
//!   excluding an account forever.
//! * **Shrinkage.** Every correction is scaled by [`confidence`], so a user with
//!   four impressions does not get a feed that reshuffles on each scroll. Thin
//!   evidence means a quiet feed, not a responsive one.
//!
//! None of these know about HTTP, the database or the wire format; they are pure
//! functions so the behaviour is testable without a running system.
//!
//! The directory mirrors those ideas — `dwell` for attention and the shared
//! shrinkage helper, `penalty` for rejections — and re-exports every item here, so
//! callers keep using `evidence::…` paths.

mod dwell;
mod penalty;

pub use dwell::{
    DWELL_CAP_MS, DWELL_CONFIDENCE_PRIOR, DWELL_MAX_CORRECTION, DWELL_WEIGHT, confidence,
    dwell_correction, shrink,
};
pub use penalty::{
    DISMISS_HALF_LIFE_DAYS, NOT_INTERESTED_HALF_LIFE_DAYS, PENALTY_CONFIDENCE_PRIOR,
    PENALTY_NORMALISER, base_penalty, decayed_penalty, normalized_penalty, penalty_half_life,
};
