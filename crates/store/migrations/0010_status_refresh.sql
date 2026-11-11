-- The first sync-token path recorded no scopes even though the consent asked for
-- `read`, so every follow sync looked unauthorised. The token is unchanged; the
-- missing fact is what we store about it.
UPDATE app_user
   SET token_scopes = ARRAY['read']
 WHERE access_token_enc IS NOT NULL
   AND (token_scopes IS NULL OR cardinality(token_scopes) = 0);

-- Accounts discovered through follows are stamped as fetched by the account upsert,
-- which used to keep the first status refresh away for a week. No status row means
-- there is nothing to keep; make them due on the next maintenance tick.
UPDATE account a
   SET last_fetched_at = NULL
 WHERE NOT EXISTS (SELECT 1 FROM status s WHERE s.account_id = a.id);
