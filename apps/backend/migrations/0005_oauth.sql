-- Lumen D1 migration 0005 — OAuth (Fase 4).
-- Renumerada: 0004 quedó para el soft-delete de dm_members (corrección Fase 2).

ALTER TABLE users ADD COLUMN email TEXT;
ALTER TABLE users ADD COLUMN oauth_provider TEXT;
ALTER TABLE users ADD COLUMN oauth_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_oauth
  ON users(oauth_provider, oauth_id) WHERE oauth_provider IS NOT NULL;

-- Anti-CSRF state para el flujo OAuth (consumido en el callback).
CREATE TABLE IF NOT EXISTS oauth_states (
  state TEXT PRIMARY KEY,
  expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_oauth_states_expiry ON oauth_states(expires_at);
