-- The federated public page is a crawler job in its own right: it seeds fresh
-- statuses for users whose affinity graph has nothing recent yet.
ALTER TABLE job DROP CONSTRAINT job_kind_valid;

ALTER TABLE job ADD CONSTRAINT job_kind_valid CHECK (
    kind IN (
        'sync_user',
        'expand_follows',
        'refresh_account',
        'refresh_profile',
        'refresh_public',
        'fetch_statuses',
        'recompute_similarity',
        'recompute_affinity',
        'recompute_follow_through',
        'retain'
    )
);
