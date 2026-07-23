-- Points paid to enter a giveaway that hasn't finished yet. Taking the points and writing
-- this row happen in one transaction, so a crash mid-giveaway can always give them back:
-- at start, every row left here is refunded. Finished giveaways delete their rows.
CREATE TABLE giveaway_spend (
    giveaway_id  TEXT NOT NULL,
    viewer_id    INTEGER NOT NULL REFERENCES viewers(id) ON DELETE CASCADE,
    amount       INTEGER NOT NULL CHECK (amount > 0),
    at           INTEGER NOT NULL,
    PRIMARY KEY (giveaway_id, viewer_id)
);
