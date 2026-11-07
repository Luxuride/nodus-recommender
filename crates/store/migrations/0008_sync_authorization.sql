-- Separate authorization for background sync.
--
-- Background sync used to seal the token the web client was already using. That made
-- the client's consent and the server's copy the same secret: revoking one revoked the
-- other, the granted scopes were whatever the client happened to hold, and a user who
-- wanted the server to sync their follows had no way to say yes to *that* and no to
-- anything wider.
--
-- The service now registers its own application on each instance and asks the user to
-- authorize it (D37). Two tables hold the pieces: the per-instance registration, whose
-- secret is sealed like any other token, and the short-lived `state` that ties a
-- callback to the user who started it.

CREATE TABLE instance_app (
    instance_host text        PRIMARY KEY,
    client_id     text        NOT NULL,
    -- Sealed with TOKEN_ENC_KEY, exactly like a user's token: it is the service's own
    -- credential against that instance and it never leaves the server.
    client_secret bytea       NOT NULL,
    redirect_uri  text        NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE sync_authorization (
    state         text        PRIMARY KEY,
    user_id       uuid        NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,
    instance_host text        NOT NULL,
    expires_at    timestamptz NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

-- A callback that never arrived must not be replayable a week later, and the sweep
-- that removes them needs an index that starts with the column it filters on.
CREATE INDEX sync_authorization_expiry_idx ON sync_authorization (expires_at);
