//! Tests for the page planner.

use super::{Plan, plan};

#[test]
fn a_page_longer_than_the_candidates_takes_them_all() {
    let plan = plan(3, 20, 1);
    assert_eq!(
        plan,
        Plan {
            head: 3,
            explore: Vec::new(),
            more: false,
        }
    );
}

#[test]
fn a_full_page_reserves_one_explore_slot() {
    let plan = plan(200, 20, 7);
    assert_eq!(plan.head, 19);
    assert_eq!(plan.explore.len(), 1);
    assert!(plan.explore[0] >= 50, "explore came from the head");
    assert!(plan.more);
}

#[test]
fn a_long_page_explores_below_its_own_head() {
    // A hundred-item page is five explore slots and a head of ninety-five, so the
    // slots cannot come from rank 51 -- the page already shows those.
    let plan = plan(400, 100, 7);
    assert_eq!(plan.head, 95);
    assert_eq!(plan.explore.len(), 5);
    assert!(
        plan.explore.iter().all(|index| *index >= 95),
        "explore overlapped the head: {:?}",
        plan.explore
    );
}

#[test]
fn explore_stops_at_the_last_rank() {
    let plan = plan(5_000, 20, 11);
    assert!(
        plan.explore.iter().all(|index| *index < 200),
        "explore went past the last rank: {:?}",
        plan.explore
    );
}

#[test]
fn the_same_seed_plans_the_same_page() {
    assert_eq!(plan(300, 40, 42), plan(300, 40, 42));
}

#[test]
fn a_different_seed_plans_a_different_page() {
    assert_ne!(plan(300, 40, 42).explore, plan(300, 40, 43).explore);
}

#[test]
fn nothing_is_left_over_when_the_page_holds_everything() {
    assert!(!plan(19, 20, 5).more);
}

#[test]
fn the_indices_are_in_page_order() {
    let plan = plan(300, 40, 9);
    let indices = plan.indices();
    assert_eq!(indices.len(), 40);
    let (head, explore) = indices.split_at(plan.head);
    assert!(head.windows(2).all(|pair| pair[1] == pair[0] + 1));
    assert!(explore.windows(2).all(|pair| pair[1] > pair[0]));
}
