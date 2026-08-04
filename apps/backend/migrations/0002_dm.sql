-- Lumen D1 migration 0002 — 1:1 DMs.
-- channels.kind gains 'dm' (SQLite can't ALTER a CHECK constraint, so the
-- table is rebuilt) and dm_members tracks the two users of each DM channel.

CREATE TABLE IF NOT EXISTS dm_members (
  channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  PRIMARY KEY (channel_id, user_id)
);
CREATE INDEX IF NOT EXISTS idx_dm_members_user ON dm_members(user_id);

-- Rebuild channels with the wider kind CHECK.
CREATE TABLE channels_new (
  id TEXT PRIMARY KEY,
  server_id TEXT REFERENCES servers(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('text', 'voice', 'dm')),
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
INSERT INTO channels_new (id, server_id, name, kind, created_at)
  SELECT id, server_id, name, kind, created_at FROM channels;
DROP TABLE channels;
ALTER TABLE channels_new RENAME TO channels;
CREATE INDEX IF NOT EXISTS idx_channels_server ON channels(server_id);
