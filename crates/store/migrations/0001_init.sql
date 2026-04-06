-- Ferrocast schema v1. Config objects (overlays, rules, goals…) are JSON documents typed
-- in Rust; tables with lookups and uniqueness rules get real columns.

CREATE TABLE settings (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- Which accounts are connected. Tokens are NOT here: they live in the OS keychain.
CREATE TABLE accounts (
    platform      TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL,
    login         TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    avatar_url    TEXT,
    scopes        TEXT NOT NULL DEFAULT '[]',
    connected_at  INTEGER NOT NULL
);

CREATE TABLE overlays (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    token       TEXT NOT NULL UNIQUE,
    width       INTEGER NOT NULL,
    height      INTEGER NOT NULL,
    widgets     TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE alert_rules (
    id          TEXT PRIMARY KEY,
    position    INTEGER NOT NULL,
    rule        TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE media (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    name        TEXT NOT NULL,
    mime        TEXT NOT NULL,
    bytes       INTEGER NOT NULL,
    sha256      TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);

-- Every normalised event. The partial unique index is the second dedupe line after the
-- in-memory window: a platform event id is stored once, ever.
CREATE TABLE events (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    id           TEXT NOT NULL UNIQUE,
    platform     TEXT NOT NULL,
    source_id    TEXT,
    kind         TEXT NOT NULL,
    origin       TEXT NOT NULL,
    occurred_at  INTEGER NOT NULL,
    received_at  INTEGER NOT NULL,
    actor_id     TEXT,
    actor_name   TEXT,
    data         TEXT NOT NULL
);
CREATE UNIQUE INDEX events_source ON events(platform, source_id) WHERE source_id IS NOT NULL;
CREATE INDEX events_kind_seq ON events(kind, seq);
CREATE INDEX events_actor ON events(platform, actor_id);

-- The alert queue's whole state, one row, rewritten on every change (R-31).
CREATE TABLE alert_queue (
    id     INTEGER PRIMARY KEY CHECK (id = 1),
    state  TEXT NOT NULL
);

CREATE TABLE goals (
    id    TEXT PRIMARY KEY,
    goal  TEXT NOT NULL
);

-- What each event added to each goal, so a refund removes exactly that (R-32).
CREATE TABLE goal_contributions (
    goal_id    TEXT NOT NULL REFERENCES goals(id) ON DELETE CASCADE,
    platform   TEXT NOT NULL,
    source_id  TEXT NOT NULL,
    amount     INTEGER NOT NULL,
    PRIMARY KEY (goal_id, platform, source_id)
);

CREATE TABLE commands (id TEXT PRIMARY KEY, command TEXT NOT NULL);
CREATE TABLE timers (id TEXT PRIMARY KEY, timer TEXT NOT NULL);
CREATE TABLE counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE quotes (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    text      TEXT NOT NULL,
    added_by  TEXT NOT NULL,
    at        INTEGER NOT NULL
);

-- One person can have accounts on several platforms (linked by hand).
CREATE TABLE viewers (
    id    INTEGER PRIMARY KEY AUTOINCREMENT,
    note  TEXT NOT NULL DEFAULT ''
);
CREATE TABLE viewer_identities (
    platform      TEXT NOT NULL,
    user_id       TEXT NOT NULL,
    viewer_id     INTEGER NOT NULL REFERENCES viewers(id) ON DELETE CASCADE,
    login         TEXT NOT NULL,
    display_name  TEXT NOT NULL,
    first_seen    INTEGER NOT NULL,
    last_seen     INTEGER NOT NULL,
    messages      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (platform, user_id)
);

CREATE TABLE tts_pending (
    id          TEXT PRIMARY KEY,
    event_id    TEXT NOT NULL,
    text        TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
