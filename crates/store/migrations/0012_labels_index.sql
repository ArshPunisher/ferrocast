-- Covers period label queries (origin = 'live' AND hidden = 0 AND kind IN (...) AND occurred_at BETWEEN ? AND ?)
-- and latest-X / reversal lookups without full table scans on the events log.
CREATE INDEX IF NOT EXISTS events_origin_hidden_kind_at
    ON events(origin, hidden, kind, occurred_at, seq);
