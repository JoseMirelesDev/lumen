-- Lumen D1 migration 0006 — moderación (Fase 5).
-- Renumerada: 0004 = dm soft delete, 0005 = oauth.

CREATE TABLE IF NOT EXISTS server_bans (
  server_id TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  reason TEXT,
  banned_by TEXT NOT NULL REFERENCES users(id),
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  PRIMARY KEY (server_id, user_id)
);
CREATE INDEX IF NOT EXISTS idx_bans_server ON server_bans(server_id);
CREATE INDEX IF NOT EXISTS idx_bans_user ON server_bans(user_id);

-- Blocks de usuarios (no DMs, no friend requests del bloqueado).
CREATE TABLE IF NOT EXISTS blocks (
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  blocked_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  PRIMARY KEY (user_id, blocked_id)
);
CREATE INDEX IF NOT EXISTS idx_blocks_blocked ON blocks(blocked_id);

CREATE TABLE IF NOT EXISTS reports (
  id TEXT PRIMARY KEY,
  reporter_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  target_type TEXT NOT NULL CHECK (target_type IN ('message', 'user', 'server')),
  target_id TEXT NOT NULL,
  reason TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX IF NOT EXISTS idx_reports_target ON reports(target_type, target_id);
