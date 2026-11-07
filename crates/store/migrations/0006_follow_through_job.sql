-- A new maintenance pass: the follow-through counters behind D29 are recomputed
-- from the event log, so the kind has to be representable in the queue before a
-- handler can claim it.
ALTER TABLE job DROP CONSTRAINT job_kind_valid;

ALTER TABLE job ADD CONSTRAINT job_kind_valid CHECK (
    kind IN (
        'sync_user',
        'expand_follows',
        'refresh_account',
        'fetch_statuses',
        'recompute_similarity',
        'recompute_affinity',
        'recompute_follow_through',
        'retain'
    )
);

COMMENT ON COLUMN job.kind IS
    'sync_user | expand_follows | refresh_account | fetch_statuses | recompute_similarity | recompute_affinity | recompute_follow_through | retain';
