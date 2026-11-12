ALTER TABLE status ADD COLUMN remote_id text;

UPDATE status SET remote_id = mastodon_id::text WHERE remote_id IS NULL;

ALTER TABLE status ALTER COLUMN remote_id SET NOT NULL;

CREATE UNIQUE INDEX status_instance_remote_key ON status (instance_host, remote_id);
