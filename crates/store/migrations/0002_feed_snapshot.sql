-- Snapshots need an identity so a second page can read back the first page's items.
--
-- Ranking happens once per feed request; a client scrolling past the first page must
-- see the same items in the same order, even if the calculator has since rewritten
-- `account_affinity`. The feed id ties those rows together, and `(user_id, feed_id, id)`
-- makes the page read a single index range.

ALTER TABLE recommendation_item
    ADD COLUMN feed_id uuid;

CREATE INDEX recommendation_item_feed_idx
    ON recommendation_item (user_id, feed_id, id)
    WHERE feed_id IS NOT NULL;

COMMENT ON COLUMN recommendation_item.feed_id IS
    'Snapshot identity shared by every item of one feed response; NULL for rows written outside the feed path';
