-- Keep the failure counters in the database, not in a process: the calculator
-- recomputes them, the feeds read them, and neither is allowed to be the only
-- copy (D16).
CREATE TABLE account_follow_through (
    account_id     bigint PRIMARY KEY REFERENCES account (id) ON DELETE CASCADE,
    impressions    integer NOT NULL DEFAULT 0,
    follows        integer NOT NULL DEFAULT 0,
    distinct_users integer NOT NULL DEFAULT 0,
    expected_rate  double precision,
    computed_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT account_follow_through_impressions_non_negative CHECK (impressions >= 0),
    CONSTRAINT account_follow_through_follows_non_negative CHECK (follows >= 0),
    CONSTRAINT account_follow_through_follows_bounded CHECK (follows <= impressions),
    CONSTRAINT account_follow_through_users_non_negative CHECK (distinct_users >= 0),
    CONSTRAINT account_follow_through_rate_sane CHECK (expected_rate IS NULL OR expected_rate BETWEEN 0.0 AND 1.0)
);

COMMENT ON TABLE account_follow_through IS
    'Evidence that an account recommendation worked: how often it was shown, how often it was followed, and the baseline it is compared against (D29).';
COMMENT ON COLUMN account_follow_through.expected_rate IS
    'Follows per impression across every account at compute time: the denominator of the lift, so it is stored rather than recomputed per request (D29).';
COMMENT ON COLUMN account_follow_through.distinct_users IS
    'How many users produced the impressions. One user clicking follow fifty times is one opinion, not fifty.';

-- Recency ordering: a recompute reads recent accounts first, and a recount
-- scan wants the newest rows when the calculator catches up after a restart.
CREATE INDEX account_follow_through_computed_idx
    ON account_follow_through (computed_at DESC);
