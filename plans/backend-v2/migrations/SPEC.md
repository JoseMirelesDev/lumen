# Migraciones D1 — Especificacion

Orden de aplicacion: `wrangler d1 migrations apply lumen-d1`

| Migration | Archivo | Contenido | Fase |
|---|---|---|---|
| 0001 | `migrations/0001_init.sql` | EXISTENTE: users, servers, server_members, channels, friendships, messages | — |
| 0002 | `migrations/0002_dm.sql` | EXISTENTE: dm_members, channels.kind += 'dm' | — |
| 0003 | `migrations/0003_crud.sql` | message_blocks, channels.topic/position, messages.edited_at/deleted_at/reply_to, servers.icon/invite_regenerated_at, users.avatar/password_version/deleted_at, refresh_tokens | Fase 1-2 |
| 0004 | `migrations/0004_dm_members_soft.sql` | **corrección Fase 2**: dm_members.deleted_at (soft delete por usuario — borrar la fila rompía la vista del otro lado) | Fase 2 |
| 0005 | `migrations/0005_oauth.sql` | users.email/oauth_provider/oauth_id, oauth_states | Fase 4 |
| 0006 | `migrations/0006_moderation.sql` | server_bans, blocks, reports | Fase 5 |
| 0007 | `migrations/0007_reactions.sql` | reactions (message_id, channel_id, user_id, emoji — sin FK a messages: los mensajes viven en blocks) | Fase 6 |
| — | (diferidos) | roles/member_roles, channel_categories, pins, FTS5 (P4): no implementados (documentados en phases/06-polish.md) | Fase 6+ |

## Restricciones D1

1. **SQLite dialect**: `strftime('%Y-%m-%dT%H:%M:%fZ', 'now')` para
   timestamps ISO. No usar `NOW()` ni `CURRENT_TIMESTAMP` sin formatear
   (darian formato distinto a los que el cliente parsea).

2. **ALTER TABLE limitado**: SQLite no permite modificar CHECK constraints
   (por eso 0002 reconstruyo channels). Evitar reconstrucciones: planificar
   el schema completo desde ahora. Nuevas restricciones CHECK -> tabla
   nueva + copy.

3. **Writes**: cada migration se cuenta como N writes (1 por fila
   modificada). Migrations en produccion con tablas grandes (>100k filas)
   pueden consumir el budget diario: aplicar fuera de horas pico.

4. **IF NOT EXISTS**: todas las tablas/indices usan `IF NOT EXISTS` para
   idempotencia (wrangler no re-aplica migrations aplicadas, pero es
   defensa en caso de errores manuales).

## Tipos de columna

| Tipo SQLite | Uso |
|---|---|
| TEXT PRIMARY KEY | IDs (UUID v4 como string) |
| TEXT | Timestamps ISO 8601, usernames, contenido, JSON |
| INTEGER | Counts, positions, bitmasks, expires_at (epoch ms) |
| BOOLEAN | No existe — usar INTEGER 0/1 |

## IDs

- Todos los IDs: UUID v4 via `crypto.randomUUID()` (Worker/DO)
- message_blocks.id: UUID
- refresh_tokens.token_hash: SHA-256 hex (no el token raw)

## Indices (cobertura completa)

```sql
-- 0001
idx_server_members_user      (user_id)
idx_channels_server          (server_id)
idx_friendships_user         (user_id)
idx_friendships_friend       (friend_id)
idx_messages_channel         (channel_id, created_at)

-- 0002
idx_dm_members_user          (user_id)

-- 0003
idx_blocks_channel           (channel_id, last_at)
idx_refresh_user             (user_id)

-- 0005
idx_bans_server              (server_id)
idx_bans_user                (user_id)
idx_blocks_blocked           (blocked_id)
idx_reports_target           (target_type, target_id)

-- 0006
idx_reactions_message        (message_id)
```

## Politica de migraciones (P6)

1. **Forward-only.** Las migraciones D1 nunca se revierten ni se editan una
   vez aplicadas en producción. SQLite `ALTER TABLE` es forward-only: una
   migración mala se corrige con una migración nueva, no borrando la vieja.
2. **Idempotencia.** `IF NOT EXISTS` en tablas/índices (defensa contra
   errores manuales; wrangler no re-aplica migrations ya aplicadas).
3. **Gates de verificación por fase:**
   - Antes de aplicar: `wrangler d1 migrations list` (estado limpio)
   - Después de aplicar: smoke suite (`pnpm smoke`) + `SELECT` de las
     tablas nuevas
   - En staging (wrangler dev con miniflare) ANTES de producción
4. **Ventana de aplicación.** Migraciones que tocan tablas con >10k filas:
   aplicar fuera de horas pico (cada fila modificada cuenta como 1 D1
   write del budget diario).
5. **Rollback = forward-fix.** Si una migración rompe producción, escribir
   `000X_fix.sql` (nunca tocar el archivo aplicado).

## Migracion a PostgreSQL (trigger Go)

Port directo con ajustes:

| SQLite | PostgreSQL |
|---|---|
| `TEXT` | `TEXT` / `UUID` / `TIMESTAMPTZ` |
| `strftime('%Y-%m-%dT%H:%M:%fZ','now')` | `now()` |
| `INSERT OR IGNORE` | `ON CONFLICT DO NOTHING` |
| FTS5 | `tsvector` + GIN index |
| `COLLATE NOCASE` | `citext` extension |
| VACUUM/ANALYZE | `VACUUM (ANALYZE)` / autovacuum |

Schema completo en `docs/schema-pg.sql` (generar al migrar).
