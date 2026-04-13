-- Moderation must survive reloads and restarts: a message the filter or a mod removed, or
-- that came from someone later banned, never returns to overlay snapshots.
ALTER TABLE events ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
CREATE INDEX events_hidden_kind_seq ON events(kind, hidden, seq);
