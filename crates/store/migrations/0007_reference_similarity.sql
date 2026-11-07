-- Reference profiles (D31).
--
-- A neighbour does not have to be one of our users. An account whose follow set we
-- have crawled can be compared against a user's follows exactly the way another user
-- is, which is what makes the taste hop usable on an instance with three signed-up
-- users and a thousand tracked accounts.
--
-- The catch is coverage: `expand_follows` walks a bounded number of pages, so a heavy
-- follower's stored follows are a prefix of the truth and cosin over a prefix
-- systematically understates similarity. The gate therefore lives in the producer
-- (`store::reference::gated`), and the coverage that admitted the row is stored beside
-- the score so a surprising neighbour can be explained.

CREATE TABLE reference_similarity (
    user_id      uuid   NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,
    account_id   bigint NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    similarity   real   NOT NULL,
    co_follow    real   NOT NULL,
    coverage     real   NOT NULL,
    computed_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, account_id),

    CONSTRAINT reference_similarity_score_range
        CHECK (similarity >= 0.0 AND similarity <= 1.0),
    CONSTRAINT reference_similarity_coverage_range
        CHECK (coverage >= 0.0 AND coverage <= 1.0)
);

COMMENT ON TABLE reference_similarity IS
    'Top neighbours among crawled accounts rather than users (D31); both sides of the pair are still needed, the user by identity and the account by follow set.';
COMMENT ON COLUMN reference_similarity.coverage IS
    'stored follows / account.following_count at the time of scoring; below the gate the row is never written.';

-- One index for the producer's freshness sweep and one for the feed-side joins.
CREATE INDEX reference_similarity_fresh_idx ON reference_similarity (computed_at DESC);

-- `account_affinity.sources` gains the generator this table feeds.
ALTER TABLE account_affinity DROP CONSTRAINT account_affinity_sources_valid;
ALTER TABLE account_affinity ADD CONSTRAINT account_affinity_sources_valid
    CHECK (sources <@ ARRAY['similar_user', 'two_hop', 'co_engagement', 'reference']::text[]);

COMMENT ON COLUMN account_affinity.sources IS
    'Generators that produced the row: similar_user, reference, two_hop, co_engagement.';
