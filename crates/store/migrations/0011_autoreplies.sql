-- Auto-replies (free, local; PRO.md M5): the bot answers chat that mentions something.
-- One row per rule, as JSON like `commands`; `position` keeps the order (first match wins).
CREATE TABLE autoreplies (
    id        TEXT PRIMARY KEY,
    position  INTEGER NOT NULL,
    rule      TEXT NOT NULL
);
CREATE INDEX autoreplies_by_position ON autoreplies(position);
