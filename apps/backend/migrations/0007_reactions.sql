-- Lumen D1 migration 0007 — Fase 6: reactions.
-- Renumerada: 0004 = dm soft delete, 0005 = oauth, 0006 = moderación.

-- Reactions keyed by message id SIN FK a messages: desde Fase 3 los mensajes
-- viven en message_blocks (JSON), no hay fila en `messages` (ADR-0004). El id
-- referencia la entrada del block; el purge de reactions de un canal borrado
-- se hace por channel_id en el delete del canal (ver db.deleteChannel).
CREATE TABLE IF NOT EXISTS reactions (
  message_id TEXT NOT NULL,
  channel_id TEXT NOT NULL,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  emoji TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  PRIMARY KEY (message_id, user_id, emoji)
);
CREATE INDEX IF NOT EXISTS idx_reactions_message ON reactions(message_id);
CREATE INDEX IF NOT EXISTS idx_reactions_channel ON reactions(channel_id);
