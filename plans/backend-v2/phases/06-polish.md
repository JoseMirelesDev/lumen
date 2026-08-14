# Fase 6 — Polish y escalado

Prioridad: **BAJA** | Dependencias: Fase 3 | Duracion estimada: continua

## Objetivo

Features de engagement y escalado que se agregan incrementalmente sin
cambios arquitectonicos. Cada item es independiente.

## Catalogo

### 6.1 Reactions

```sql
CREATE TABLE reactions (
  message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  emoji TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  PRIMARY KEY (message_id, user_id, emoji)
);
CREATE INDEX idx_reactions_message ON reactions(message_id);
```

```
PUT    /api/messages/:id/reactions/:emoji   -> toggle
GET    /api/channels/:id/messages/reactions -> agregado al block
```

Broadcast de reaction via PresenceHubDO (tag c:channelId).

### 6.2 Message replies

```
POST /api/channels/:id/messages  { content, replyTo? }
```

- `reply_to` ya esta en schema (Fase 2)
- UI: "responder" -> quote del mensaje original

### 6.3 Pins

```sql
ALTER TABLE messages ADD COLUMN pinned_at TEXT;
```

```
POST /api/messages/:id/pin    (owner del server o author)
DELETE /api/messages/:id/pin
GET  /api/channels/:id/pins
```

### 6.4 Attachments

```
POST /api/channels/:id/messages  { content?, attachment? }
PUT  /api/uploads (presigned)  -> R2 key
```

- Upload a R2 con key `attachments/<messageId>/<filename>`
- El mensaje guarda `attachmentUrl` (nueva columna)
- 25 MB max por archivo

### 6.5 Search (D1 LIKE / FTS5)

SQLite FTS5 disponible en D1:

```sql
CREATE VIRTUAL TABLE messages_fts USING fts5(
  content, content='messages', content_rowid='rowid'
);
```

```
GET /api/search?q=...&serverId=...  -> [message]
```

Limitacion: FTS en D1 es viable para <1M filas. Mas alla -> Go/Postgres
tsvector. Documentar como techo.

### 6.6 Roles y permisos

```sql
CREATE TABLE roles (
  id TEXT PRIMARY KEY,
  server_id TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  permissions INTEGER NOT NULL DEFAULT 0,  -- bitmask
  position INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE member_roles (
  server_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  role_id TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
  PRIMARY KEY (server_id, user_id, role_id)
);
```

Bitmask de permisos:

```typescript
const PERMS = {
  MANAGE_CHANNELS: 1 << 0,
  MANAGE_MEMBERS: 1 << 1,
  MANAGE_MESSAGES: 1 << 2,
  SEND_MESSAGES: 1 << 3,
  VOICE: 1 << 4,
  BAN_MEMBERS: 1 << 5,
  MANAGE_ROLES: 1 << 6,
  MANAGE_SERVER: 1 << 7,
};
```

Chequeo en el Worker: `hasPerm(server, user, PERM)` via query de roles
(cacheada 30s). Default: miembros = SEND_MESSAGES | VOICE; owner = all.

### 6.7 Channel categories

```sql
CREATE TABLE channel_categories (
  id TEXT PRIMARY KEY,
  server_id TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  name TEXT NOT NULL,
  position INTEGER NOT NULL DEFAULT 0
);
ALTER TABLE channels ADD COLUMN category_id TEXT REFERENCES channel_categories(id);
```

### 6.8 Search de usuarios

```
GET /api/users/search?q=...  -> [User]  (limit 10, rate 10/min)
```

`WHERE username LIKE ?` con índice ya existente (UNIQUE NOCASE).

### 6.9 Admin panel

```
GET  /api/admin/reports            (env ADMIN_IDS contains user)
POST /api/admin/reports/:id/resolve
```

Env var: `ADMIN_IDS = "user1,user2"`.

### 6.10 Migracion a Go (trigger: > 4,000 usuarios activos)

Cuando Worker requests o DO requests superen el 70% del limite por 7 dias:

1. Mantener CF como CDN/proxy (Cloudflare Tunnel a la VM)
2. Go server: REST + WS hub + PostgreSQL (misma API, mismo protocolo WS)
3. D1 -> PostgreSQL (schema port, pg_dump style)
4. R2 queda en CF (Go lo llama via S3 API)
5. Descomisionar Worker y DOs gradualmente

El protocolo WS y la API REST son identicos — el cliente no cambia. Solo
cambia la URL base. Los tipos de `@lumen/protocol` se comparten.

---

## Orden sugerido de implementacion (6.x)

| Orden | Item | Por que |
|---|---|---|
| 1 | Replies | UX comun, barato (campo ya existe) |
| 2 | Attachments | Alta demanda, R2 ya montado |
| 3 | Reactions | Engagement |
| 4 | Pins | Util |
| 5 | Search | FTS5, techo documentado |
| 6 | Roles | Mayor complejidad, antes de crecer |
| 7 | Categories | Organizacion |
| 8 | User search | Simple |
| 9 | Admin panel | Trust & safety |
| 10 | Migracion Go | Trigger por metrica |

## Budget (estimaciones por item)

| Item | Worker req/dia | D1 writes/dia | D1 reads/dia |
|---|---|---|---|
| Reactions | +500 | +1,000 | +500 |
| Replies | 0 (campo extra) | 0 | 0 |
| Attachments | +500 (uploads) | +250 | +500 |
| Pins | +50 | +50 | +50 |
| Search | +200 | 0 | +500 |
| Roles | +100 | +100 (rare) | +1,000 (cache 30s) |
| Categories | +20 | +20 | 0 |
| User search | +50 | 0 | +50 |
| Admin | +10 | +10 | +100 |
| **Total** | **~1,430** | **~1,930** | **~2,700** |

Todo dentro del headroom calculado en BUDGET.md (aun al 15-20% del total
con 500 users).

## Aceptacion (por item)

Cada item 6.x se acepta con:
- Smoke test de la ruta
- Test de permisos (quien puede hacer que)
- Medicion de budget (una linea en BUDGET.md)
- Actualizacion de protocol.md y README.md del plan
