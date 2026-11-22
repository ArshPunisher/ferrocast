CREATE TABLE api_keys (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    prefix      TEXT NOT NULL,
    key_hash    TEXT NOT NULL UNIQUE,
    scopes_json TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER,
    last_used   INTEGER,
    revoked_at  INTEGER
);

CREATE INDEX idx_api_keys_created ON api_keys(created_at DESC);

CREATE TABLE api_key_audit (
    id       TEXT PRIMARY KEY,
    key_id   TEXT NOT NULL,
    key_name TEXT NOT NULL,
    method   TEXT NOT NULL,
    path     TEXT NOT NULL,
    status   INTEGER NOT NULL,
    at       INTEGER NOT NULL
);

CREATE INDEX idx_api_key_audit_at ON api_key_audit(at DESC);
