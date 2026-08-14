-- Lumen D1 migration 0003 — CRUD completo + message blocks + soft deletes
-- (Fase 1: refresh_tokens · Fase 2: CRUD). Forward-only: nunca editar una vez
-- aplicada; correcciones = migración nueva (migrations/SPEC.md).

-- Message blocks (chat buffer flush, ADR-0004): 1 fila = JSON array de hasta
-- 50 mensajes. Escritura SOLO desde el PresenceHubDO (Fase 3).
CREATE TABLE IF NOT EXISTS message_blocks (
  id TEXT PRIMARY KEY,
  channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  messages TEXT NOT NULL,          -- JSON array de BufferedMessage
  count INTEGER NOT NULL,
  first_at TEXT NOT NULL,
  last_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_blocks_channel ON message_blocks(channel_id, last_at);

-- Channels: topic + position (orden manual en el server).
ALTER TABLE channels ADD COLUMN topic TEXT;
ALTER TABLE channels ADD COLUMN position INTEGER NOT NULL DEFAULT 0;

-- Messages: edit + soft delete + replies (ADR-0010: mutaciones vía el DO).
ALTER TABLE messages ADD COLUMN edited_at TEXT;
ALTER TABLE messages ADD COLUMN deleted_at TEXT;
ALTER TABLE messages ADD COLUMN reply_to TEXT REFERENCES messages(id);

-- Servers: icon (R2 key) + tracking de regeneración de invite.
ALTER TABLE servers ADD COLUMN icon TEXT;
ALTER TABLE servers ADD COLUMN invite_regenerated_at TEXT;

-- Users: avatar (R2 key), password version (re-hash futuro), soft delete.
ALTER TABLE users ADD COLUMN avatar TEXT;
ALTER TABLE users ADD COLUMN password_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE users ADD COLUMN deleted_at TEXT;

-- Refresh tokens (Fase 1, ADR-0007): SHA-256 del token, revocables, rotados.
CREATE TABLE IF NOT EXISTS refresh_tokens (
  token_hash TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  expires_at TEXT NOT NULL,
  revoked_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_refresh_user ON refresh_tokens(user_id);
