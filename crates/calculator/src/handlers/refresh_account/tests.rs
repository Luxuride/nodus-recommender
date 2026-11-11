//! The pure half of `refresh_account`: visibility, counts and timestamp mapping.
//!
//! Nothing here touches the network or the database. The walk itself is a thin
//! loop over [`map_status`], so pinning the mapping pins the only rule that can
//! silently ingest a post we must not keep.

use nodus_instance::model::{Account, Status, Visibility};

use super::fetch::map_status;

fn author() -> Account {
    Account {
        id: "1".to_owned(),
        acct: "alice".to_owned(),
        display_name: "Alice".to_owned(),
        note: String::new(),
        avatar: String::new(),
        header: String::new(),
        bot: false,
        locked: false,
        discoverable: None,
        indexable: None,
        noindex: None,
        followers_count: 0,
        following_count: 0,
        statuses_count: 0,
        last_status_at: None,
    }
}

fn status(id: &str, visibility: Visibility) -> Status {
    Status {
        url: None,
        sensitive: false,
        spoiler_text: String::new(),
        media_attachments: Vec::new(),
        mentions: Vec::new(),
        tags: Vec::new(),
        poll: None,
        edited_at: None,
        pinned: None,
        id: id.to_owned(),
        created_at: "2026-11-06T21:00:00.000Z".to_owned(),
        content: "<p>hello</p>".to_owned(),
        visibility,
        account: author(),
        favourites_count: 3,
        reblogs_count: 2,
        replies_count: 1,
        favourited: false,
        reblogged: false,
        bookmarked: false,
        reblog: None,
        in_reply_to_id: None,
        emojis: Vec::new(),
    }
}

#[test]
fn a_private_or_direct_status_is_dropped() {
    for visibility in [Visibility::Private, Visibility::Direct] {
        assert!(
            map_status("example.social", 7, &status("110", visibility)).is_none(),
            "{visibility:?} must not be retained"
        );
    }
}

#[test]
fn a_public_status_keeps_its_counts_and_visibility() {
    let mapped = map_status("example.social", 7, &status("110", Visibility::Public))
        .expect("a public status maps");

    assert_eq!(mapped.instance_host, "example.social");
    assert_eq!(mapped.account_id, 7);
    assert_eq!(mapped.mastodon_id, 110);
    assert_eq!(mapped.visibility, "public");
    assert_eq!(mapped.favourites_count, 3);
    assert_eq!(mapped.reblogs_count, 2);
    assert_eq!(mapped.replies_count, 1);
    assert_eq!(mapped.language, None);
    assert_eq!(mapped.published_at.year(), 2026);
}

#[test]
fn an_unparseable_numeric_field_falls_back_instead_of_panicking() {
    let mut huge = status("110", Visibility::Unlisted);
    huge.favourites_count = u64::MAX;
    huge.reblogs_count = u64::MAX;
    huge.replies_count = u64::MAX;

    let mapped = map_status("example.social", 7, &huge).expect("a huge count still maps");
    assert_eq!(mapped.favourites_count, i32::MAX);
    assert_eq!(mapped.reblogs_count, i32::MAX);
    assert_eq!(mapped.replies_count, i32::MAX);
    assert_eq!(mapped.visibility, "unlisted");

    let unnamed = status("not-a-number", Visibility::Public);
    assert!(map_status("example.social", 7, &unnamed).is_none());
}
