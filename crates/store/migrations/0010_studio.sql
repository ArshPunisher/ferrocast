-- Studio (streamer tools): stream markers (big moments, chat spikes, clips) with the stream
-- they belong to, and watch time per account for `!watchtime`. Counters and quotes already
-- have their tables (0001); counter definitions and tool settings are JSON in `settings`.
CREATE TABLE markers (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    at                 INTEGER NOT NULL,
    -- When the stream it belongs to started (NULL when nothing was live).
    stream_started_at  INTEGER,
    kind               TEXT NOT NULL,
    label              TEXT NOT NULL,
    platform           TEXT,
    clip               INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX markers_by_time ON markers(at);

CREATE TABLE watch_time (
    platform    TEXT NOT NULL,
    user_id     TEXT NOT NULL,
    minutes     INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (platform, user_id)
);
