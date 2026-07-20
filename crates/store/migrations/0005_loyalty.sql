-- Loyalty points, one balance per person (a viewer row), so linked accounts on Twitch,
-- YouTube and Kick share it. The CHECK is the last line of defence against a negative
-- balance; the repo already refuses to go below zero.
CREATE TABLE loyalty_balances (
    viewer_id   INTEGER PRIMARY KEY REFERENCES viewers(id) ON DELETE CASCADE,
    points      INTEGER NOT NULL DEFAULT 0 CHECK (points >= 0),
    updated_at  INTEGER NOT NULL
);
CREATE INDEX loyalty_top ON loyalty_balances(points DESC);

-- Support events that already paid a bonus: a redelivered follow or sub pays once (R-29).
CREATE TABLE loyalty_awards (
    platform   TEXT NOT NULL,
    source_id  TEXT NOT NULL,
    at         INTEGER NOT NULL,
    PRIMARY KEY (platform, source_id)
);

-- `!addpoints @name` finds people by login.
CREATE INDEX viewer_identities_login ON viewer_identities(platform, login COLLATE NOCASE);
