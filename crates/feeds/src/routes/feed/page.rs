//! Which candidates make up a page: the head of the ranking, plus its explore slots.
//!
//! Kept as a pure index computation on the candidate count rather than a function
//! over `Candidate`, because the decision is about *positions* and nothing about the
//! rows: the head is whatever the ranking put first, and the explore slots are drawn
//! from a rank window below it. Working on indices makes the whole thing testable
//! without building fifty stored accounts.

use nodus_core::explore;
use nodus_store::feed::Candidate;

/// The exploration window, restated here so the planner owns the invariant it
/// enforces: explore slots start below [`nodus_core::explore::EXPLORE_FIRST_RANK`]
/// and come from no deeper than [`nodus_core::explore::EXPLORE_LAST_RANK`].
const FIRST_RANK: usize = explore::EXPLORE_FIRST_RANK;
const LAST_RANK: usize = explore::EXPLORE_LAST_RANK;

/// One page's worth of decisions: how many head candidates, and which of the rest.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Plan {
    /// Number of leading candidates to take: `candidates[..head]`.
    pub head: usize,
    /// Indices into `candidates` for the explore slots, ascending.
    pub explore: Vec<usize>,
    /// Whether the candidate list held anything beyond this page.
    pub more: bool,
}

impl Plan {
    /// The selected indices in page order: head first, then explore.
    ///
    /// Explore items are appended rather than interleaved because the head is the
    /// answer to the user's request and the slots are evidence gathering; mixing them
    /// would make the page's quality depend on where a coin landed.
    pub fn indices(&self) -> Vec<usize> {
        (0..self.head).chain(self.explore.iter().copied()).collect()
    }
}

/// Plans a page of `limit` items over `available` ranked candidates.
///
/// `seed` is the feed's identity, so the same snapshot plans the same page on every
/// replica and on every reload.
pub(super) fn plan(available: usize, limit: usize, seed: u64) -> Plan {
    let wanted = explore::slots(limit).min(available);
    let head = limit.saturating_sub(wanted).min(available);

    // The pool starts below both the window and the head: a hundred-item page takes
    // ranks 1..95 as its head, so its explore slots must come from 95.. and not from
    // an account the page already shows.
    let pool_start = FIRST_RANK.max(head).min(available);
    let pool_len = available
        .saturating_sub(pool_start)
        .min(LAST_RANK.saturating_sub(pool_start));

    if pool_len == 0 {
        return Plan {
            head,
            explore: Vec::new(),
            more: available > head,
        };
    }

    let mut explore: Vec<usize> = explore::choose(pool_len, wanted, seed)
        .into_iter()
        .map(|offset| pool_start + offset)
        .collect();
    explore.sort_unstable();

    let more = available > head + explore.len();
    Plan {
        head,
        explore,
        more,
    }
}

/// The candidates a plan selects, in page order.
pub(super) fn select<'a>(candidates: &'a [Candidate], plan: &Plan) -> Vec<&'a Candidate> {
    plan.indices()
        .into_iter()
        .filter_map(|index| candidates.get(index))
        .collect()
}

#[cfg(test)]
mod tests;
