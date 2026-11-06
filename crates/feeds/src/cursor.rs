//! Opaque keyset cursors.
//!
//! A cursor names a position *inside one snapshot*: the feed id plus the last item id
//! served. Two properties matter and both are why this is not just an offset:
//!
//! - page 2 must answer from the snapshot page 1 created, not from a fresh ranking,
//!   so the list cannot reshuffle while the reader scrolls;
//! - an item id is monotonic within the snapshot, so `id > cursor` is one index range
//!   and does not need the score at all.
//!
//! The encoding is base64url without padding so a cursor survives a query string
//! unchanged. Nothing about it is secret; it is opaque only to discourage clients from
//! constructing one.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use uuid::Uuid;

/// Encodes `(feed_id, item_id)` as a cursor.
#[must_use]
pub fn encode(feed_id: Uuid, item_id: i64) -> String {
    URL_SAFE_NO_PAD.encode(format!("{feed_id}:{item_id}"))
}

/// Decodes a cursor, returning `None` for anything we did not produce.
#[must_use]
pub fn decode(cursor: &str) -> Option<(Uuid, i64)> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (feed, item) = text.split_once(':')?;
    Some((Uuid::parse_str(feed).ok()?, item.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_round_trips() {
        let feed = Uuid::now_v7();
        let cursor = encode(feed, 42);
        assert_eq!(decode(&cursor), Some((feed, 42)));
    }

    #[test]
    fn a_cursor_is_url_safe() {
        let cursor = encode(Uuid::now_v7(), i64::MAX);
        assert!(
            cursor
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "cursor {cursor} contains characters that need escaping"
        );
    }

    #[test]
    fn junk_is_not_a_cursor() {
        for junk in [
            "",
            "!!!",
            "bm90LWEtY3Vyc29y",
            "Zm9vOmJhcg",
            "Zm9vOmJhcjpwb3N0",
        ] {
            assert_eq!(decode(junk), None, "{junk:?} decoded");
        }
    }
}
