-- Importers (StreamElements, Streamlabs). Points for people Ferrocast hasn't seen yet wait
-- here and move to their balance the first time they show up: by platform user id when the
-- import had one, else by unique login (Twitch, Kick). Never by display name, which anyone
-- can set. YouTube names aren't unique, so a YouTube entry without a channel id is kept with
-- `needs_match = 1` and never paid automatically.
CREATE TABLE pending_points (
    platform    TEXT NOT NULL,
    login       TEXT NOT NULL COLLATE NOCASE,
    user_id     TEXT,
    needs_match INTEGER NOT NULL DEFAULT 0,
    points      INTEGER NOT NULL CHECK (points > 0),
    source      TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (platform, login)
);

-- Tip history from another service, kept for the streamer's records (and the tips.csv
-- export). Separate from the event log so years of old tips never flood the activity feed,
-- the recap or goals. One row per tip, ever: importing twice changes nothing.
CREATE TABLE imported_tips (
    source        TEXT NOT NULL,
    source_id     TEXT NOT NULL,
    occurred_at   INTEGER NOT NULL,
    name          TEXT NOT NULL,
    amount_minor  INTEGER NOT NULL CHECK (amount_minor >= 0),
    currency      TEXT NOT NULL,
    message       TEXT,
    provider      TEXT NOT NULL,
    imported_at   INTEGER NOT NULL,
    PRIMARY KEY (source, source_id)
);
