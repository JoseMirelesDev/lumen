# Fase 2 — CRUD completo

Prioridad: **CRITICA** | Dependencias: ninguna | Duracion estimada: 3-4 dias

## Objetivo

Completar las operaciones de gestion que faltan: editar/borrar servers,
channels, mensajes, perfil, amigos. Sin esto la app no es usable en
produccion (no puedes borrar nada).

## Archivos

| Archivo | Cambio |
|---|---|
| `src/index.ts` | Nuevas rutas |
| `src/db.ts` | Nuevas queries |
| `migrations/0003_crud.sql` | message_blocks, topics, soft deletes, refresh_tokens |
| `src/validation.ts` | Nuevos validators |
| `packages/protocol/src/index.ts` | Tipos nuevos |
| `test/auth.test.ts` / `test/smoke-ws.mjs` | Tests nuevos |

## Tareas

### 2.1 Schema: migration 0003_crud.sql

```sql
-- Migration 0003 — CRUD completo + message blocks + soft deletes

-- Message blocks (chat buffer flush)
CREATE TABLE IF NOT EXISTS message_blocks (
  id TEXT PRIMARY KEY,
  channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  messages TEXT NOT NULL,
  count INTEGER NOT NULL,
  first_at TEXT NOT NULL,
  last_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_blocks_channel ON message_blocks(channel_id, last_at);

-- Channels: topic + position
ALTER TABLE channels ADD COLUMN topic TEXT;
ALTER TABLE channels ADD COLUMN position INTEGER NOT NULL DEFAULT 0;

-- Messages: edit + soft delete + replies
ALTER TABLE messages ADD COLUMN edited_at TEXT;
ALTER TABLE messages ADD COLUMN deleted_at TEXT;
ALTER TABLE messages ADD COLUMN reply_to TEXT;

-- Servers: icon + invite tracking
ALTER TABLE servers ADD COLUMN icon TEXT;
ALTER TABLE servers ADD COLUMN invite_regenerated_at TEXT;

-- Users: avatar + password version + soft delete
ALTER TABLE users ADD COLUMN avatar TEXT;
ALTER TABLE users ADD COLUMN password_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE users ADD COLUMN deleted_at TEXT;

-- Refresh tokens (Fase 1)
CREATE TABLE IF NOT EXISTS refresh_tokens (
  token_hash TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  expires_at TEXT NOT NULL,
  revoked_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_refresh_user ON refresh_tokens(user_id);
```

### 2.2 Rutas nuevas

#### Perfil

```
PATCH /api/me                      { username? }  -> { user }
PUT   /api/me/password             { currentPassword, newPassword } -> 204
DELETE /api/me                     -> 204 (soft delete + revoke sessions)
```

Reglas:
- username: validar `validateUsername`, UNIQUE check, error 409 si ocupado
- password: `verifyPassword(current)` primero, luego hash nuevo
- delete: `UPDATE users SET deleted_at = now` + revoke all refresh tokens.
  El login chequea `deleted_at IS NULL`. Cascade a servers donde es owner:
  transferir o marcar `servers.deleted_at` (decidir: en v1, servers del
  usuario borrado quedan huérfanos con `owner_deleted_at`; el cliende
  muestra "server sin owner" y otro miembro no puede gestionarlo — Fase 5
  agrega transferencia forzada).

#### Servers

```
PATCH  /api/servers/:id            { name?, icon? }        -> { server }   (owner)
DELETE /api/servers/:id            -> 204 (owner, cascade)
POST   /api/servers/:id/leave      -> 204 (cualquier miembro no-owner)
POST   /api/servers/:id/invite     -> { inviteCode }       (owner, regenera)
DELETE /api/servers/:id/members/:userId -> 204 (owner kick)
```

Reglas:
- DELETE server: cascade a channels, messages (blocks incluidos), members,
  bans. Confirmar con `?confirm=true` (anti-accidental).
- leave: el owner NO puede dejar el server (debe transferir o borrar).
- kick: no al owner. Implementar en Fase 5 el ban (kick+baneo).

#### Channels

```
PATCH  /api/channels/:id           { name?, topic?, position? } -> { channel } (owner)
DELETE /api/channels/:id           -> 204 (owner, cascade messages)
```

Reglas:
- DMs no se borran via esta ruta (solo `DELETE /api/dms/:id` en Fase 5).
- position: reordenar con un PUT batch en Fase 3.

#### Messages

```
PATCH  /api/messages/:id           { content }  -> { message }  (author)
DELETE /api/messages/:id           -> 204       (author o owner del server)
```

> **PROVISIONAL (ADR-0010):** en Fase 3 estas rutas se reemplazan por
> `chat-edit` / `chat-delete` vía WS del PresenceHubDO (los mensajes viven
> en blocks, ADR-004, y el DO serializa las escrituras). Fase 2 las
> implementa contra la tabla `messages` para tener el CRUD completo ya;
> Fase 3 las elimina y el REST queda read-only.

Reglas:
- edit: solo author, solo si `deleted_at IS NULL`, set `edited_at`.
- delete: soft delete (`deleted_at`) — el mensaje queda como placeholder
  "mensaje eliminado" en el block. broadcast de invalidation via
  PresenceHubDO (Fase 3) para online users.
- En Fase 3 (buffer), PATCH/DELETE de un mensaje que aun esta en el
  buffer: el PresenceHubDO debe poder editar/borrar del buffer tambien.
  Para Fase 2 (aun REST directo a messages), no hay buffer todavia.

#### Amigos

```
DELETE /api/friends/:userId         -> 204 (remove friend)
```

Reglas: borra ambas filas de friendships (la (A,B) y (B,A)). En schema
actual, la amistad aceptada existe como 2 filas? Revisar: `friendships`
tiene UNIQUE(user_id, friend_id) y el accept crea las 2 direcciones
(depende de implementacion actual — verificar en db.ts `acceptFriendRequest`).

#### DMs

```
DELETE /api/dms/:id                -> 204 (cualquiera de los 2, soft)
```

Regla: borra dm_members row del que borra (el otro lo sigue viendo).
Asi no se pierde historial del otro lado.

### 2.3 db.ts: queries nuevas

```typescript
// servers
export async function updateServer(db, id, patch): Promise<ServerRow | null>;
export async function deleteServer(db, id): Promise<void>;          // hard cascade
export async function removeMember(db, serverId, userId): Promise<void>;
export async function regenerateInvite(db, serverId, code): Promise<void>;
export async function isOwner(db, serverId, userId): Promise<boolean>;

// channels
export async function updateChannel(db, id, patch): Promise<ChannelRow | null>;
export async function deleteChannel(db, id): Promise<void>;

// messages
export async function updateMessageContent(db, id, content): Promise<TextMessage | null>;
export async function softDeleteMessage(db, id): Promise<void>;

// users
export async function updateUsername(db, id, username): Promise<void>;
export async function updatePassword(db, id, salt, hash): Promise<void>;
export async function softDeleteUser(db, id): Promise<void>;
export async function updateAvatar(db, id, key): Promise<void>;

// friends
export async function removeFriendship(db, userId, friendId): Promise<void>;

// dms
export async function removeDmMember(db, channelId, userId): Promise<void>;
```

### 2.4 protocol: tipos nuevos

```typescript
// en packages/protocol/src/index.ts
export interface MessageBlock {
  id: string;
  channelId: string;
  count: number;
  firstAt: string;
  lastAt: string;
}

export interface EditMessageResult {
  id: string;
  content: string;
  editedAt: string;
}

export type ServerRole = "owner" | "member";
```

### 2.5 Tests

| Test | Que verifica |
|---|---|
| Server CRUD | create -> patch -> delete -> 404 |
| Server permissions | member no puede patch/delete -> 403 |
| Channel CRUD | create -> patch topic -> delete |
| Message edit/delete | author edita; otro no (403); owner del server borra |
| Friend remove | remove -> no aparece en /api/friends |
| User delete | login -> 401, refresh revocado |
| Password change | old password falla, new funciona |
| Username conflict | 409 |
| DM delete | borra para uno, sigue para el otro |

## Cambios al cliente (Fase 2)

| Cambio | Archivo |
|---|---|
| Menu context del server (editar, invitar, dejar, borrar) | `apps/lumen-slint/ui/` |
| Menu context del channel (editar, borrar) | `apps/lumen-slint/ui/` |
| Menu context del mensaje (editar, borrar) | `apps/lumen-slint/ui/` |
| Panel de amigos: "quitar amigo" | `apps/lumen-slint/ui/` |
| Settings de perfil (username, password) | `apps/lumen-slint/ui/` |
| API calls nuevas | `crates/lumen-core/src/api.rs` |
| Estado: edited/deleted en mensajes | `crates/lumen-core/src/model.rs` |

## Aceptacion

- [ ] `pnpm smoke` pasa con las nuevas rutas
- [ ] CRUD de servers/channels/messages/friends funciona end-to-end
- [ ] Permisos: solo owner edita/borra server; author edita mensaje
- [ ] Cascade: borrar server borra channels y messages (blocks)
- [ ] Budget: sin impacto (mismo patron de writes que CRUD existente)

## Riesgos

| Riesgo | Mitigacion |
|---|---|
| Soft delete en messages + blocks JSON | El placeholder se marca en el JSON del block; al leer, filtrar `deleted_at` y mostrar placeholder |
| Editar mensaje en block vs buffer | Fase 3 resuelve con invalidation broadcast; en Fase 2 el edit se hace directo a la fila (bloqueado si el mensaje aun no fue flusheado — documentar) |
| Deleting user con servers owned | Soft delete + flag; el server queda visible pero sin owner accionable hasta Fase 5 |
