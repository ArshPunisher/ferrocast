-- When each account last chatted (not just was seen: a follow also counts as seen), so the
-- dashboard can mark someone's first message ever and someone coming back after a while.
ALTER TABLE viewer_identities ADD COLUMN last_message_at INTEGER;
UPDATE viewer_identities SET last_message_at = last_seen WHERE messages > 0;
