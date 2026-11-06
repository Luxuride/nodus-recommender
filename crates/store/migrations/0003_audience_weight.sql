-- D24 turns "how many people could see this account" into a refresh interval
-- (hot >= 25 users' affinity sets, warm >= 1, cold otherwise), page depth, and the
-- scheduler's ordering. That count is a GROUP BY over `account_affinity`, which a
-- scheduler cannot afford per candidate and cannot order an index scan by, so it
-- is denormalised onto the account row it describes.
--
-- The column is derived state: `recompute_affinity` overwrites it, nothing else
-- writes it, and a reader must not treat it as a live COUNT(*). A NOT NULL column
-- with a constant default is metadata-only on Postgres 11+, so this does not
-- rewrite the largest table in the schema.

ALTER TABLE account
    ADD COLUMN audience_weight integer NOT NULL DEFAULT 0;

-- D24 orders a tier by `audience_weight DESC, last_refreshed_at ASC`; the column
-- the schema stamps on a fetch is `account.last_fetched_at` (D20), so that is what
-- this index sorts on. NULLS FIRST keeps never-fetched accounts at the head, as
-- `account_refresh_sweep_idx` does. Moved accounts are never refreshed again, so
-- they are out of the index entirely.
CREATE INDEX account_refresh_order_idx
    ON account (audience_weight DESC, last_fetched_at ASC NULLS FIRST)
    WHERE moved_to_account_id IS NULL;

COMMENT ON COLUMN account.audience_weight IS
    'Denormalised count of app_users whose affinity set contains this account; maintained by recompute_affinity (the recompute_similarity pass), never a live COUNT(*). Stale by at most one recompute cycle, see docs/09-operations.md.';
