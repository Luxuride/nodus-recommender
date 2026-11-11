-- Two separate choices on Mastodon: discovery features and search-engine indexing.
-- `noindex` is the older inverse spelling; keeping both avoids guessing an instance's
-- version and lets the newest observation win.
ALTER TABLE account ADD COLUMN indexable boolean;
ALTER TABLE account ADD COLUMN noindex boolean;

COMMENT ON COLUMN account.indexable IS 'Whether the account opted into search-engine indexing.';
COMMENT ON COLUMN account.noindex IS 'Deprecated inverse of indexable, when an instance still reports it.';
