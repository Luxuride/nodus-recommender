-- The id the author's own instance uses for the account. A local shadow id means
-- nothing on the author's host, so public links need the origin id.
ALTER TABLE account ADD COLUMN remote_id text;

COMMENT ON COLUMN account.remote_id IS
    'The account id on the author''s own instance, learned by account lookup.';
