-- Lumen D1 migration 0004 — soft delete de DM por usuario.
--
-- Corrección (forward-only, migrations/SPEC.md): Fase 2 definió "borrar el DM
-- borrando la fila de dm_members", pero listDmChannelsForUser resuelve el
-- nombre del otro participante con un JOIN a su fila — borrarla hacía
-- desaparecer el canal para AMBOS. El marcador deleted_at mantiene la vista
-- del otro lado (el historial no se pierde) y el re-open restaura la fila.

ALTER TABLE dm_members ADD COLUMN deleted_at TEXT;
CREATE INDEX IF NOT EXISTS idx_dm_members_active
  ON dm_members(channel_id, user_id) WHERE deleted_at IS NULL;
