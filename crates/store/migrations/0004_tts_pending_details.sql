-- The approval list shows who sent a message and what they paid without looking up the
-- event, which may never be stored (test events) or may be pruned (old chat).
ALTER TABLE tts_pending ADD COLUMN platform TEXT;
ALTER TABLE tts_pending ADD COLUMN name TEXT;
ALTER TABLE tts_pending ADD COLUMN amount TEXT;
