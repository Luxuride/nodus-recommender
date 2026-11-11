-- The recommendation card is the status card: it needs the same media, warning and
-- custom-emoji data the timeline has, or it renders a different, lesser post.
ALTER TABLE status
    ADD COLUMN media jsonb NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN sensitive boolean NOT NULL DEFAULT false,
    ADD COLUMN spoiler_text text NOT NULL DEFAULT '',
    ADD COLUMN emojis jsonb NOT NULL DEFAULT '[]'::jsonb;

COMMENT ON COLUMN status.media IS 'MediaAttachment array, serialized as the instance returned it.';
COMMENT ON COLUMN status.emojis IS 'CustomEmoji array used by the post body.';
