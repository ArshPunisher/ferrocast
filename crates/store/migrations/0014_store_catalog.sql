-- Community Theme Store installed packages and persistent one-click undo snapshots (R37).
CREATE TABLE IF NOT EXISTS store_installs (
    item_id        TEXT PRIMARY KEY,
    kind           TEXT NOT NULL,
    format         TEXT NOT NULL,
    title          TEXT NOT NULL,
    version        TEXT NOT NULL,
    creator_id     TEXT NOT NULL,
    creator_name   TEXT NOT NULL,
    package_sha256 TEXT NOT NULL,
    design_ids     TEXT NOT NULL,
    rule_ids       TEXT NOT NULL,
    plugin_id      TEXT,
    installed_at   INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS store_undo (
    token          TEXT PRIMARY KEY,
    action         TEXT NOT NULL,
    item_id        TEXT NOT NULL,
    item_title     TEXT NOT NULL,
    snapshot_json  TEXT NOT NULL,
    created_at     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS store_undo_created_at ON store_undo(created_at DESC);
