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

#[test]
fn a_decayed_rejection_takes_an_explore_slot() {
    use nodus_store::feed::Candidate;
    use time::OffsetDateTime;

    fn candidate() -> Candidate {
        Candidate {
            account_id: 1,
            affinity: 0.5,
            support: 1,
            sources: Vec::new(),
            computed_at: OffsetDateTime::UNIX_EPOCH,
            last_shown_at: None,
            seen_attention_7d: 0.0,
            rejected_kind: None,
            rejected_at: None,
            impressions: 0,
            follow_through_shown: None,
            follow_through_followed: None,
            expected_follow_rate: None,
            acct: "a@b".to_owned(),
            display_name: None,
            note: None,
            avatar_url: None,
            followers_count: 0,
            bot: false,
            last_status_at: None,
            instance_host: "b".to_owned(),
        }
    }

    let candidates = vec![candidate(); 200];
    let mut plan = plan(candidates.len(), 20, 7);
    let before = plan.explore.clone();

    let promoted = super::promote_recovery(&mut plan, &candidates, |_| false);
    assert!(!promoted, "nothing to promote");
    assert_eq!(plan.explore, before);

    let promoted = super::promote_recovery(&mut plan, &candidates, |candidate| {
        candidate.account_id == 1
    });
    assert!(
        promoted,
        "a recovering candidate below the head takes a slot"
    );
    assert!(
        plan.explore.contains(&plan.head),
        "the first candidate below the head took the slot: {:?}",
        plan.explore
    );
    assert_eq!(plan.explore.len(), before.len(), "the page did not grow");
}

#[test]
fn a_recovering_candidate_the_page_already_shows_is_not_promoted() {
    use nodus_store::feed::Candidate;
    use time::OffsetDateTime;

    let candidate = Candidate {
        account_id: 1,
        affinity: 0.5,
        support: 1,
        sources: Vec::new(),
        computed_at: OffsetDateTime::UNIX_EPOCH,
        last_shown_at: None,
        seen_attention_7d: 0.0,
        rejected_kind: Some("dismiss".to_owned()),
        rejected_at: None,
        impressions: 0,
        follow_through_shown: None,
        follow_through_followed: None,
        expected_follow_rate: None,
        acct: "a@b".to_owned(),
        display_name: None,
        note: None,
        avatar_url: None,
        followers_count: 0,
        bot: false,
        last_status_at: None,
        instance_host: "b".to_owned(),
    };
    let candidates: Vec<_> = (0..200)
        .map(|index| {
            let mut copy = candidate.clone();
            copy.account_id = index;
            copy
        })
        .collect();
    let mut plan = plan(candidates.len(), 20, 7);
    let before = plan.explore.clone();

    // Only the head's own candidate is flagged, and the page already shows it.
    let promoted = super::promote_recovery(&mut plan, &candidates, |candidate| {
        candidate.account_id == 3
    });
    assert!(
        !promoted,
        "a candidate inside the head is already on the page"
    );
    assert_eq!(plan.explore, before);
}
