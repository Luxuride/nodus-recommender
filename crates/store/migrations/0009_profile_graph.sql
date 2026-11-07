-- A profile is the unit of comparison, not a user.
--
-- Until now similarity could only compare `app_user` to `app_user`, because
-- `user_similarity.other_user_id` references that table. But the recommender never
-- needed a signed-in user — it needed a weighted set of edges. This migration gives
-- every account a profile, whether it is one of ours (`server_sync`, the token was
-- ours) or somebody we crawled (`public_follow`, visible because they did not set
-- `hide_collections`).
--
-- The weights in `nodus_core::similarity` already assume exactly these signals
-- (co-follow 0.55, co-boost 0.15, co-reply 0.10), so the maths does not change; what
-- changes is who may be a neighbour.

CREATE TABLE profile_edge (
    account_id bigint NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    kind text NOT NULL,
    target_account_id bigint NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    source text NOT NULL,
    observed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, kind, target_account_id),
    CONSTRAINT profile_edge_kind_valid CHECK (kind IN ('follow', 'boost', 'reply')),
    CONSTRAINT profile_edge_source_valid CHECK (
        source IN ('server_sync', 'public_follow', 'public_boost', 'public_reply')
    ),
    -- A profile never points at itself, and this is the database's job: the crawler
    -- resolving ids from a page cannot rule it out on its own.
    CONSTRAINT profile_edge_not_self CHECK (account_id <> target_account_id)
);

-- "Who does this account follow" (the crawler's read) and "who else follows them"
-- (the neighbour query) are both indexed; the second is the one that decides how
-- long a similarity pass takes.
CREATE INDEX profile_edge_target_idx ON profile_edge (target_account_id, kind);
CREATE INDEX profile_edge_kind_source_idx ON profile_edge (kind, source);

CREATE TABLE profile_similarity (
    account_id bigint NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    peer_id bigint NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    score real NOT NULL,
    co_follow real NOT NULL DEFAULT 0,
    co_boost real NOT NULL DEFAULT 0,
    co_reply real NOT NULL DEFAULT 0,
    computed_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, peer_id),
    CONSTRAINT profile_similarity_not_self CHECK (account_id <> peer_id)
);

CREATE INDEX profile_similarity_peer_idx ON profile_similarity (peer_id);

-- When the graph was last crawled. `NULL` means never, and it is the column the
-- "only crawl what is needed" rule reads: an account nobody follows and nobody
-- recommends stays NULL.
ALTER TABLE account ADD COLUMN graph_fetched_at timestamptz;

-- The candidate query keeps its shape: `account_affinity` is still what the feed
-- reads, so a profile-derived recommendation and a user-derived one are
-- indistinguishable downstream.
COMMENT ON COLUMN account.graph_fetched_at IS
    'When this account''s follow graph was last crawled; NULL means never.';

-- The crawler is a new job kind, so the queue has to be able to represent it before
-- a handler can claim it. `job_subject_kind_valid` already allows `account`.
ALTER TABLE job DROP CONSTRAINT job_kind_valid;

ALTER TABLE job ADD CONSTRAINT job_kind_valid CHECK (
    kind IN (
        'sync_user',
        'expand_follows',
        'refresh_account',
        'refresh_profile',
        'fetch_statuses',
        'recompute_similarity',
        'recompute_affinity',
        'recompute_follow_through',
        'retain'
    )
);

COMMENT ON COLUMN job.kind IS
    'sync_user | expand_follows | refresh_account | refresh_profile | fetch_statuses | recompute_similarity | recompute_affinity | recompute_follow_through | retain';
