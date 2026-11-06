-- nodus initial schema (v1).
--
-- Applied by `sqlx migrate run`; sqlx wraps each migration in a transaction,
-- so this file deliberately opens none.
-- Enum-like columns are `text` + named CHECK constraints so that adding a
-- value later is an additive migration (ALTER ... DROP/ADD CONSTRAINT) and
-- never a type rewrite. Extensions, then tables in dependency order, then
-- indexes, then comments.

CREATE EXTENSION IF NOT EXISTS citext;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- ---------------------------------------------------------------------------
-- Tables
-- ---------------------------------------------------------------------------

CREATE TABLE instance (
    host                 citext PRIMARY KEY,
    software             text,
    software_version     text,
    title                text,
    rate_limit_per_5min  integer,
    blocked_at           timestamptz,
    last_contact_at      timestamptz,
    created_at           timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE app_user (
    id                    uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    instance_host         citext NOT NULL REFERENCES instance (host) ON DELETE CASCADE,
    mastodon_account_id   bigint NOT NULL,
    acct                  citext NOT NULL,
    access_token_enc      bytea,
    token_scopes          text[],
    token_fingerprint     bytea,
    sync_enabled          boolean NOT NULL DEFAULT false,
    telemetry_opt_in      boolean NOT NULL DEFAULT false,
    telemetry_opt_in_at   timestamptz,
    settings              jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at            timestamptz NOT NULL DEFAULT now(),
    last_seen_at          timestamptz NOT NULL DEFAULT now(),
    deleted_at            timestamptz,
    CONSTRAINT app_user_instance_account_key UNIQUE (instance_host, mastodon_account_id),
    CONSTRAINT app_user_sync_requires_token
        CHECK (access_token_enc IS NOT NULL OR sync_enabled = false)
);

CREATE TABLE account (
    id                    bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    instance_host         citext NOT NULL REFERENCES instance (host) ON DELETE CASCADE,
    mastodon_id           bigint NOT NULL,
    acct                  citext NOT NULL,
    display_name          text,
    note                  text,
    avatar_url            text,
    bot                   boolean NOT NULL DEFAULT false,
    locked                boolean NOT NULL DEFAULT false,
    followers_count       integer NOT NULL DEFAULT 0,
    following_count       integer NOT NULL DEFAULT 0,
    statuses_count        integer NOT NULL DEFAULT 0,
    last_status_at        timestamptz,
    follows_private       boolean NOT NULL DEFAULT false,
    moved_to_account_id   bigint REFERENCES account (id) ON DELETE SET NULL,
    discoverable          boolean,
    lang                  text,
    first_seen_at         timestamptz NOT NULL DEFAULT now(),
    last_fetched_at       timestamptz,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT account_instance_mastodon_key UNIQUE (instance_host, mastodon_id),
    CONSTRAINT account_moved_to_not_self
        CHECK (moved_to_account_id IS NULL OR moved_to_account_id <> id)
);

CREATE TABLE follow_edge (
    follower_id    bigint NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    followee_id    bigint NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    source         text NOT NULL,
    first_seen_at  timestamptz NOT NULL DEFAULT now(),
    last_seen_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (follower_id, followee_id),
    CONSTRAINT follow_edge_source_valid
        CHECK (source IN ('server_sync', 'request_scoped', 'expansion')),
    CONSTRAINT follow_edge_no_self_follow CHECK (follower_id <> followee_id)
);

CREATE TABLE engagement_edge (
    actor_id       bigint NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    target_id      bigint NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    kind           text NOT NULL,
    count          integer NOT NULL DEFAULT 1,
    first_seen_at  timestamptz NOT NULL DEFAULT now(),
    last_seen_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (actor_id, target_id, kind),
    CONSTRAINT engagement_edge_kind_valid
        CHECK (kind IN ('favourite', 'reblog', 'reply')),
    CONSTRAINT engagement_edge_count_positive CHECK (count > 0)
);

CREATE TABLE status (
    id                bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    instance_host     citext NOT NULL REFERENCES instance (host) ON DELETE CASCADE,
    mastodon_id       bigint NOT NULL,
    account_id        bigint NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    content           text,
    visibility        text NOT NULL,
    favourites_count  integer NOT NULL DEFAULT 0,
    reblogs_count     integer NOT NULL DEFAULT 0,
    replies_count     integer NOT NULL DEFAULT 0,
    in_reply_to_id    bigint,
    language          text,
    url               text,
    created_at        timestamptz NOT NULL,
    fetched_at        timestamptz NOT NULL DEFAULT now(),
    expires_at        timestamptz NOT NULL DEFAULT (now() + interval '14 days'),
    CONSTRAINT status_instance_mastodon_key UNIQUE (instance_host, mastodon_id),
    CONSTRAINT status_visibility_retained
        CHECK (visibility IN ('public', 'unlisted'))
);

CREATE TABLE user_similarity (
    user_id        uuid NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,
    other_user_id  uuid NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,
    similarity     real NOT NULL,
    co_follow      real NOT NULL,
    co_fav         real NOT NULL,
    co_reply       real NOT NULL,
    reciprocity    real NOT NULL,
    computed_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, other_user_id),
    CONSTRAINT user_similarity_distinct_users CHECK (user_id <> other_user_id)
);

CREATE TABLE account_affinity (
    user_id      uuid NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,
    account_id   bigint NOT NULL REFERENCES account (id) ON DELETE CASCADE,
    affinity     real NOT NULL,
    support      smallint NOT NULL DEFAULT 0,
    sources      text[] NOT NULL DEFAULT '{}',
    computed_at  timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL DEFAULT (now() + interval '7 days'),
    PRIMARY KEY (user_id, account_id),
    CONSTRAINT account_affinity_sources_valid
        CHECK (sources <@ ARRAY['similar_user', 'two_hop', 'co_engagement']::text[])
);

CREATE TABLE recommendation_item (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id        uuid NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,
    account_id     bigint REFERENCES account (id) ON DELETE CASCADE,
    status_id      bigint REFERENCES status (id) ON DELETE CASCADE,
    kind           text NOT NULL,
    score          real NOT NULL,
    reason         jsonb NOT NULL DEFAULT '{}'::jsonb,
    model_version  integer NOT NULL,
    served_at      timestamptz NOT NULL DEFAULT now(),
    expires_at     timestamptz NOT NULL DEFAULT (now() + interval '7 days'),
    CONSTRAINT recommendation_item_kind_valid
        CHECK (kind IN ('account', 'post')),
    CONSTRAINT recommendation_item_target_matches_kind CHECK (
        (kind = 'account' AND account_id IS NOT NULL AND status_id IS NULL)
        OR (kind = 'post' AND status_id IS NOT NULL AND account_id IS NULL)
    )
);

CREATE TABLE interaction_event (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    user_id          uuid NOT NULL REFERENCES app_user (id) ON DELETE CASCADE,
    item_id          bigint NOT NULL REFERENCES recommendation_item (id) ON DELETE CASCADE,
    kind             text NOT NULL,
    client_ts        timestamptz NOT NULL,
    server_ts        timestamptz NOT NULL DEFAULT now(),
    dwell_ms         integer,
    idempotency_key  bytea NOT NULL,
    expires_at       timestamptz NOT NULL DEFAULT (now() + interval '90 days'),
    CONSTRAINT interaction_event_user_idempotency_key UNIQUE (user_id, idempotency_key),
    CONSTRAINT interaction_event_kind_valid CHECK (
        kind IN ('impression', 'click', 'follow', 'dismiss',
                 'not_interested', 'boost', 'reply')
    ),
    CONSTRAINT interaction_event_dwell_nonnegative
        CHECK (dwell_ms IS NULL OR dwell_ms >= 0)
);

CREATE TABLE job (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    kind          text NOT NULL,
    priority      smallint NOT NULL,
    subject_kind  text NOT NULL,
    subject_id    text,
    payload       jsonb NOT NULL DEFAULT '{}'::jsonb,
    status        text NOT NULL DEFAULT 'pending',
    attempts      smallint NOT NULL DEFAULT 0,
    max_attempts  smallint NOT NULL DEFAULT 5,
    run_after     timestamptz NOT NULL DEFAULT now(),
    locked_at     timestamptz,
    locked_by     text,
    last_error    text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    finished_at   timestamptz,
    CONSTRAINT job_kind_valid CHECK (
        kind IN ('sync_user', 'expand_follows', 'refresh_account',
                 'fetch_statuses', 'recompute_similarity',
                 'recompute_affinity', 'retain')
    ),
    CONSTRAINT job_priority_valid CHECK (priority BETWEEN 0 AND 4),
    CONSTRAINT job_subject_kind_valid
        CHECK (subject_kind IN ('user', 'account', 'instance', 'none')),
    CONSTRAINT job_status_valid
        CHECK (status IN ('pending', 'running', 'done', 'failed')),
    CONSTRAINT job_attempts_within_max CHECK (attempts <= max_attempts)
);

CREATE TABLE model_version (
    version     integer PRIMARY KEY,
    weights     jsonb NOT NULL,
    trained_at  timestamptz NOT NULL DEFAULT now(),
    auc         real,
    samples     integer,
    active      boolean NOT NULL DEFAULT false,
    notes       text,
    CONSTRAINT model_version_auc_range
        CHECK (auc IS NULL OR (auc >= 0 AND auc <= 1))
);

-- ---------------------------------------------------------------------------
-- Indexes
-- ---------------------------------------------------------------------------

-- Live users only: the sync scheduler and support tooling never read
-- soft-deleted rows, so the index stays small.
CREATE INDEX app_user_live_last_seen_idx
    ON app_user (last_seen_at DESC)
    WHERE deleted_at IS NULL;

-- Handle search ("alice@exa...") via trigram matching.
CREATE INDEX account_acct_trgm_idx
    ON account USING gin (acct gin_trgm_ops);

-- 7-day metadata refresh sweep: never-fetched and stalest accounts first.
CREATE INDEX account_refresh_sweep_idx
    ON account (last_fetched_at NULLS FIRST);

-- FK: ON DELETE SET NULL needs the referencing rows, not the null ones.
CREATE INDEX account_moved_to_idx
    ON account (moved_to_account_id)
    WHERE moved_to_account_id IS NOT NULL;

-- Reverse path ("who follows X", needed for reasons and similarity) plus
-- FK support for followee_id; follower_id is included to avoid a heap visit.
CREATE INDEX follow_edge_followee_idx
    ON follow_edge (followee_id) INCLUDE (follower_id);

-- Reverse lookup ("who engaged with X") plus FK support for target_id.
CREATE INDEX engagement_edge_target_idx
    ON engagement_edge (target_id, kind);

-- Post feed: latest N statuses per author.
CREATE INDEX status_account_created_idx
    ON status (account_id, created_at DESC);

-- Retention: batched range delete by deadline.
CREATE INDEX status_expires_at_idx
    ON status (expires_at);

-- Candidate generation: top-K similar users for a user.
CREATE INDEX user_similarity_user_score_idx
    ON user_similarity (user_id, similarity DESC);

-- FK support for the second user column.
CREATE INDEX user_similarity_other_user_idx
    ON user_similarity (other_user_id);

-- Accounts feed read path: highest-affinity candidates for a user.
CREATE INDEX account_affinity_user_score_idx
    ON account_affinity (user_id, affinity DESC);

-- FK support: deleting an account drops its affinity rows.
CREATE INDEX account_affinity_account_idx
    ON account_affinity (account_id);

-- Recent served items per user (feedback attribution, frequency capping).
CREATE INDEX recommendation_item_user_served_idx
    ON recommendation_item (user_id, served_at DESC);

-- FK support; both target columns are sparse by kind.
CREATE INDEX recommendation_item_account_idx
    ON recommendation_item (account_id)
    WHERE account_id IS NOT NULL;

CREATE INDEX recommendation_item_status_idx
    ON recommendation_item (status_id)
    WHERE status_id IS NOT NULL;

-- Retention: batched range delete by deadline.
CREATE INDEX recommendation_item_expires_at_idx
    ON recommendation_item (expires_at);

-- Recent events per user, ordered by our receive clock (client clocks lie).
CREATE INDEX interaction_event_user_server_ts_idx
    ON interaction_event (user_id, server_ts DESC);

-- FK support for item_id.
CREATE INDEX interaction_event_item_idx
    ON interaction_event (item_id);

-- Retention: batched range delete by deadline.
CREATE INDEX interaction_event_expires_at_idx
    ON interaction_event (expires_at);

-- Claim query: FOR UPDATE SKIP LOCKED over due pending work, best tier first.
CREATE INDEX job_claim_idx
    ON job (priority, run_after)
    WHERE status = 'pending';

-- At most one outstanding (pending or running) job per subject. subject_id is
-- NULL for subject_kind = 'none', and NULLs are distinct in a unique index, so
-- singleton jobs are intentionally not deduplicated.
CREATE UNIQUE INDEX job_outstanding_subject_idx
    ON job (kind, subject_kind, subject_id)
    WHERE status IN ('pending', 'running');

-- Lease reaper: find running jobs whose 60 s lease expired.
CREATE INDEX job_lease_reaper_idx
    ON job (locked_at)
    WHERE status = 'running';

-- Cleanup: delete finished rows after 7 days.
CREATE INDEX job_finished_at_idx
    ON job (finished_at)
    WHERE finished_at IS NOT NULL;

-- At most one active weight set.
CREATE UNIQUE INDEX model_version_single_active_idx
    ON model_version (active)
    WHERE active;

-- ---------------------------------------------------------------------------
-- Comments
-- ---------------------------------------------------------------------------

COMMENT ON TABLE instance IS 'Fediverse hosts we talk to, with crawl policy and discovered rate limits.';
COMMENT ON COLUMN instance.rate_limit_per_5min IS 'Discovered X-RateLimit-Limit; null until the first successful call.';
COMMENT ON COLUMN instance.blocked_at IS 'Non-null means policy forbids crawling this host; null means allowed.';
COMMENT ON COLUMN instance.last_contact_at IS 'Last successful request; drives backoff when a host goes quiet.';

COMMENT ON TABLE app_user IS 'Our signed-in users; the only accounts we may hold a token for.';
COMMENT ON COLUMN app_user.access_token_enc IS 'XChaCha20-Poly1305 nonce||ciphertext of the sync token copy; NULL means no background sync.';
COMMENT ON COLUMN app_user.token_scopes IS 'Scopes granted to the stored token copy; NULL when no token is stored.';
COMMENT ON COLUMN app_user.token_fingerprint IS 'sha256 prefix of the token, used as the verify-cache key; not reversible to the token.';
COMMENT ON COLUMN app_user.sync_enabled IS 'Server-side sync consent; requires a stored token (see app_user_sync_requires_token).';
COMMENT ON COLUMN app_user.telemetry_opt_in IS 'Gates every interaction_event write for this user.';
COMMENT ON COLUMN app_user.telemetry_opt_in_at IS 'When the current opt-in was given; null when never opted in.';
COMMENT ON COLUMN app_user.settings IS 'Per-user preferences as JSON so new flags need no migration.';
COMMENT ON COLUMN app_user.deleted_at IS 'Soft delete; the row and its dependent data are purged 30 days later.';

COMMENT ON TABLE account IS 'Every fediverse account we know about, keyed by (instance, mastodon id).';
COMMENT ON COLUMN account.note IS 'Bio, truncated to 2 KB by the app; the full field is not needed for ranking.';
COMMENT ON COLUMN account.follows_private IS 'Instance hides this account''s follower collection (hide_collections); it cannot be expanded.';
COMMENT ON COLUMN account.moved_to_account_id IS 'Successor account after an instance move; null when not moved.';
COMMENT ON COLUMN account.discoverable IS 'Tri-state: null means the instance did not report it.';
COMMENT ON COLUMN account.last_status_at IS 'Recency of the last post; drives freshness scoring.';
COMMENT ON COLUMN account.last_fetched_at IS 'Last metadata fetch; drives the 7-day refresh sweep.';
COMMENT ON COLUMN account.updated_at IS 'Maintained by the writer; no trigger by design.';

COMMENT ON TABLE follow_edge IS 'Who follows whom as observed by us; the backbone of similarity.';
COMMENT ON COLUMN follow_edge.source IS 'How the edge was observed: own-user sync, request-scoped read or 2-hop expansion.';
COMMENT ON COLUMN follow_edge.last_seen_at IS 'Refreshed on re-sync; stale edges are decayed and pruned.';

COMMENT ON TABLE engagement_edge IS 'Aggregated engagement by our users'' accounts; private signal, never exposed per user.';
COMMENT ON COLUMN engagement_edge.target_id IS 'Author of the engaged status, not the status itself.';
COMMENT ON COLUMN engagement_edge.count IS 'Number of distinct engagements of this kind observed.';

COMMENT ON TABLE status IS 'Recent posts from candidate authors, retained only while public or unlisted.';
COMMENT ON COLUMN status.content IS 'Plaintext post body, truncated to 2 KB by the app; no HTML.';
COMMENT ON COLUMN status.visibility IS 'Retention rule made structural: only public and unlisted rows may exist.';
COMMENT ON COLUMN status.created_at IS 'Remote timestamp from the instance, not our fetch time.';
COMMENT ON COLUMN status.in_reply_to_id IS 'Remote status id; deliberately not a FK because the parent may never be ingested.';
COMMENT ON COLUMN status.fetched_at IS 'When we last fetched or refreshed the row.';
COMMENT ON COLUMN status.expires_at IS '14-day POST_RETENTION_DAYS deadline; the retain job deletes by this column.';

COMMENT ON TABLE user_similarity IS 'Per-user similarity to other users, recomputed nightly; top 50 kept per user.';
COMMENT ON COLUMN user_similarity.similarity IS 'Combined 0..1 score; the component signals are stored separately for explainability.';
COMMENT ON COLUMN user_similarity.reciprocity IS 'Mutual-follow component of the score.';

COMMENT ON TABLE account_affinity IS 'Precomputed candidate account scores per user so a feed request is a read.';
COMMENT ON COLUMN account_affinity.support IS 'How many distinct similar users back this candidate; feeds the "followed by N" copy.';
COMMENT ON COLUMN account_affinity.sources IS 'Generators that produced the row: similar_user, two_hop, co_engagement.';
COMMENT ON COLUMN account_affinity.expires_at IS '7-day TTL; stale rows are recomputed, not served.';

COMMENT ON TABLE recommendation_item IS 'What we actually served, so feedback can be attributed and explained.';
COMMENT ON COLUMN recommendation_item.account_id IS 'Set for kind = account; null for post items.';
COMMENT ON COLUMN recommendation_item.status_id IS 'Set for kind = post; null for account items.';
COMMENT ON COLUMN recommendation_item.reason IS 'Explainability payload ({code, support, sample_followers}) rendered by the UI.';
COMMENT ON COLUMN recommendation_item.model_version IS 'model_version.version that scored the item; not a FK so old versions can be pruned.';
COMMENT ON COLUMN recommendation_item.served_at IS 'When the item was returned to the client.';
COMMENT ON COLUMN recommendation_item.expires_at IS '7-day attribution window; deleted by the retain job.';

COMMENT ON TABLE interaction_event IS 'Client telemetry for opted-in users; the behavioural signal behind ranking.';
COMMENT ON COLUMN interaction_event.item_id IS 'The served item this event refers to; required in v1.';
COMMENT ON COLUMN interaction_event.idempotency_key IS 'sha256 of (item_id, kind, client_ts); makes client retries safe.';
COMMENT ON COLUMN interaction_event.client_ts IS 'Client clock, server-clamped on insert.';
COMMENT ON COLUMN interaction_event.server_ts IS 'Our receive time; the ordering key for frequency capping.';
COMMENT ON COLUMN interaction_event.dwell_ms IS 'Coarse client-measured dwell time; null when not measured.';
COMMENT ON COLUMN interaction_event.expires_at IS '90-day retention deadline; deleted by the retain job.';

COMMENT ON TABLE job IS 'Durable work queue drained with FOR UPDATE SKIP LOCKED; no broker.';
COMMENT ON COLUMN job.kind IS 'Job type; the tier (P0..P4) is carried by priority.';
COMMENT ON COLUMN job.priority IS 'P0 (0) .. P4 (4); the claim query takes the lowest value first.';
COMMENT ON COLUMN job.subject_id IS 'uuid or bigint rendered as text so one column serves user, account and instance subjects.';
COMMENT ON COLUMN job.payload IS 'Job-specific arguments, e.g. {"account_id": 42}.';
COMMENT ON COLUMN job.run_after IS 'Earliest claim time; used for backoff and scheduling.';
COMMENT ON COLUMN job.locked_at IS 'Start of the 60-second lease held by the claiming worker.';
COMMENT ON COLUMN job.locked_by IS 'Worker identity holding the lease; the reaper requeues expired leases.';
COMMENT ON COLUMN job.updated_at IS 'Maintained by the claiming worker; no trigger by design.';
COMMENT ON COLUMN job.finished_at IS 'Set for done and failed rows; cleanup deletes rows older than 7 days.';
COMMENT ON COLUMN job.attempts IS 'Claim attempts so far; compared against max_attempts.';

COMMENT ON TABLE model_version IS 'Ranking weights are data, not constants; one row is active at a time.';
COMMENT ON COLUMN model_version.weights IS 'Named coefficients (w_sim, w_support, w_fresh, ...).';
COMMENT ON COLUMN model_version.auc IS 'Offline validation AUC at promotion time; null when not evaluated.';
COMMENT ON COLUMN model_version.samples IS 'Training/validation sample count behind this version.';
COMMENT ON COLUMN model_version.active IS 'Exactly one row may be true; enforced by model_version_single_active_idx.';
