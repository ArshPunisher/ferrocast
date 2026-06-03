-- Messages the filter held for a mod (R-48): kept until a mod releases or drops them, so a
-- restart doesn't lose the queue. A held message is also hidden until it is released.
ALTER TABLE events ADD COLUMN held INTEGER NOT NULL DEFAULT 0;
CREATE INDEX events_held ON events(held) WHERE held = 1;
-- How often we timed someone out, for the viewer card.
ALTER TABLE viewer_identities ADD COLUMN timeouts INTEGER NOT NULL DEFAULT 0;
