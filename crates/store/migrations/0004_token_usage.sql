-- The token pool's round-robin clock (D23). This is when a token was last
-- *picked*, not an audit trail of what it fetched: `pick_token` orders by it so a
-- host with several consented accounts rotates instead of draining the first one.
ALTER TABLE app_user ADD COLUMN token_last_used_at timestamptz;

COMMENT ON COLUMN app_user.token_last_used_at IS
    'Round-robin clock for the token pool (D23): the last time this token was picked, not an audit log.';

-- Matches the selection predicate in `users::pick_token`: live, sync-enabled rows
-- on one host, ordered by the pool clock. Partial, so rows that can never be
-- picked (no token, sync off, soft-deleted) do not take up index space.
CREATE INDEX app_user_token_pool_idx
    ON app_user (instance_host, token_last_used_at NULLS FIRST)
    WHERE sync_enabled AND access_token_enc IS NOT NULL AND deleted_at IS NULL;
