# ARCHITECTURE.md — Diseno tecnico completo

## 1. Topologia de Durable Objects

### 1.1 PresenceHubDO (singleton, NUEVO)

Un solo DO para toda la presencia. Instance name: `"hub"`.

```
id = LUMEN_PRESENCE_DO.idFromName("hub")
```

**Por que singleton:**

| Criterio | Singleton | Per-server | Per-user |
|----------|-----------|------------|----------|
| WS per usuario | 1 | N (1 por server) | 1 |
| Friend status cross-server | Gratis | Complejo | Complejo |
| DO requests en connect | 1 | N | 1 |
| Memoria (1000 users) | ~2 MB | ~2 MB total | ~2 MB total |
| Storage | ~0 | ~0 | ~0 |
| Complejidad routing | Baja | Alta | Media |

A escala free tier (<5,000 users) el singleton es estrictamente superior.

**Tags por socket:**

```
acceptWebSocket(server, [
  userId,                        // "alice"
  `s:${serverId}`,               // "s:s1", "s:s2" (un tag por server)
])
```

**Lookups sin storage:**

| Operacion | Llamada |
|---|---|
| User online? | `getWebSockets(userId).length > 0` |
| Server members online | `getWebSockets("s:" + serverId)` |
| Data del socket | `socket.deserializeAttachment()` |

**Attachment shape:**

```typescript
interface PresenceAttachment {
  userId: string;
  username: string;
  status: "online" | "idle" | "dnd";
  servers: string[];          // serverIds donde es miembro (validado en upgrade)
  friends: string[];          // userIds amigos (validado en upgrade)
  voiceChannelId: string | null;
  voiceServerId: string | null;
  // Rate limiting
  msgWindowStart: number;
  msgCount: number;
  typingWindowStart: number;
  typingCount: number;
}
```

**Zero state.storage para presencia.** Los buffers de chat usan
`state.storage` con keys `buf:<channelId>` (unico uso de storage).

### 1.2 ChannelDO (existente, sin cambios funcionales)

Voice signaling. `MAX_PEERS = 4` se mantiene (mesh).

---

## 2. Protocolo de presencia (WS /api/presence)

### 2.1 Upgrade (Worker -> DO)

```
GET /api/presence?token=<jwt>

Worker:
  1. verifyToken(jwt) -> userId
  2. SELECT servers, friends FROM users JOIN ... (1 D1 read)
  3. fetch PresenceHubDO con query params:
     ?userId&username&servers=s1,s2&friends=f1,f2
  4. DO: acceptWebSocket + tags + attachment
  5. DO responde 101 (WebSocket)
```

Nota: listas pequenas (<= 50 servers, <= 200 friends) caben en query
params sin problema. Para listas mayores, mover a headers custom.

### 2.2 Mensajes client -> DO (PresenceClientMessage)

```typescript
type PresenceClientMessage =
  | { type: "ready" }                          // ya conectado, pedir snapshot
  | { type: "status"; status: "online" | "idle" | "dnd" }
  | { type: "voice-join"; channelId: string; serverId: string }
  | { type: "voice-leave" }
  | { type: "chat"; channelId: string; serverId: string; content: string; clientId: string }
  | { type: "typing"; channelId: string; serverId: string }
  | { type: "subscribe"; channelId: string }   // empezar a recibir chat RT
  | { type: "unsubscribe"; channelId: string }
  | { type: "dm-signal"; to: string; sdp?: string; candidate?: unknown; kind: "offer" | "answer" | "ice" }
  | { type: "ping" };
```

### 2.3 Mensajes DO -> client (PresenceServerMessage)

```typescript
type PresenceServerMessage =
  | { type: "ready";
      onlineFriends: { userId: string; username: string; status: string }[];
      servers: {
        serverId: string;
        onlineMembers: { userId: string; username: string }[];
        voiceChannels: { channelId: string; peers: { userId: string; username: string }[] }[];
      }[];
    }
  | { type: "friend-online"; userId: string; username: string }
  | { type: "friend-offline"; userId: string }
  | { type: "friend-status"; userId: string; status: string }
  | { type: "voice-update"; serverId: string; channelId: string;
      peers: { userId: string; username: string }[] }
  | { type: "member-online"; serverId: string; userId: string; username: string }
  | { type: "member-offline"; serverId: string; userId: string }
  | { type: "chat"; channelId: string;
      message: { id: string; authorId: string; authorName: string;
                  content: string; createdAt: string } }
  | { type: "chat-ack"; clientId: string; messageId: string; createdAt: string }
  | { type: "chat-error"; clientId: string; code: string }
  | { type: "dm-offer"; from: string; sdp: string }
  | { type: "dm-answer"; from: string; sdp: string }
  | { type: "dm-ice"; from: string; candidate: unknown }
  | { type: "pong" }
  | { type: "error"; code: string; message: string };
```

### 2.4 Broadcast routing (gratis, via tags)

| Evento | Receptores | Implementacion |
|---|---|---|
| `friend-online` | Los amigos online del que conecta | `getWebSockets(friendId)` por cada amigo |
| `friend-offline` | Los amigos online del que desconecta | `getWebSockets(friendId)` por cada amigo |
| `voice-update` | Miembros del server | `getWebSockets("s:" + serverId)` |
| `member-online` | Miembros del server | `getWebSockets("s:" + serverId)` |
| `chat` (RT) | Suscritos al channel | tag `c:<channelId>` (nuevo tag al subscribirse) |
| `typing` | Suscritos al channel | tag `c:<channelId>` |

---

## 3. Chat: buffer + flush + paginacion

### 3.1 Flujo del mensaje

```
client --WS--> PresenceHubDO
 1. checkRate (10 msgs / 10s, en attachment)
 2. Validar serverId en attachment.servers
 3. Validar channelId existe y pertenece al server (Worker hizo esta
    validacion al cargar el server; el cliente envia serverId con cada msg)
 4. state.storage: buf:<channelId> push message
 5. Broadcast chat a suscritos via tag c:<channelId>
 6. ACK al sender
 7. Si buf.length >= 50 -> flushChannel
    Si no hay alarm -> setAlarm(now + 5 min)
```

### 3.2 Flush (1 D1 write por block)

```typescript
async flushChannel(channelId: string, messages: BufferedMessage[]) {
  await this.env.LUMEN_D1.prepare(
    `INSERT INTO message_blocks (id, channel_id, messages, count, first_at, last_at)
     VALUES (?, ?, ?, ?, ?, ?)`
  ).bind(
    crypto.randomUUID(), channelId,
    JSON.stringify(messages), messages.length,
    messages[0].createdAt, messages[messages.length - 1].createdAt
  ).run();
  await this.state.storage.delete(`buf:${channelId}`);
}
```

### 3.3 Paginacion (1 D1 read por pagina)

Cursor compuesto `(last_at, id)` — `last_at` solo puede colisionar
(milisegundos), el id desempata.

```sql
-- Pagina mas reciente
SELECT id, messages, count, first_at, last_at
FROM message_blocks
WHERE channel_id = ?
ORDER BY last_at DESC, id DESC
LIMIT 1;

-- Pagina anterior (cursor = (last_at, id) mas viejo ya cargado)
SELECT id, messages, count, first_at, last_at
FROM message_blocks
WHERE channel_id = ? AND (last_at, id) < (?, ?)
ORDER BY last_at DESC, id DESC
LIMIT 1;
```

Merge con buffer: el PresenceHubDO expone `GET https://do/buffer/:channelId`
que devuelve los mensajes pendientes. El Worker los mergea y ordena.

### 3.4 Mutaciones de mensajes (ADR-0010)

**Todas las mutaciones (send, edit, delete) pasan por el PresenceHubDO vía
WS** — el DO es single-threaded y serializa toda escritura sobre un
channel. El REST queda read-only para messages.

- `chat-edit` / `chat-delete` con ACK (mismo patrón que `chat`, ADR-005)
- Edit en buffer: modificar la entrada. Edit flusheado: leer block,
  modificar JSON, reescribir (1 read + 1 write, serializado por el DO)
- Delete: marcar `deleted: true` en el entry → placeholder en lectura
- Broadcast de invalidación a suscritos (tag `c:<channelId>`)
- Los `PATCH/DELETE /api/messages/:id` de Fase 2 son PROVISIONALES y se
  reemplazan por WS en Fase 3

Client UX:

```
Apertura channel:
  GET /api/channels/:id/messages?before=<cursor o null>
  -> [100 mensajes mas recientes]
  -> WS subscribe channelId
  -> WS: mensajes nuevos llegan en tiempo real

Scroll up:
  GET /api/channels/:id/messages?before=<cursor viejo>
  -> [100 mensajes anteriores]

Editar/borrar:
  PATCH/DELETE /api/messages/:id  (REST, luego broadcast de invalidation
  via PresenceHubDO para refrescar a online users)
```

---

## 4. P2P DataChannels

### 4.1 DM data channel

```
Alice abre DM con Bob:
 1. Alice: WS presence -> { dm-signal, to: bob, kind: "offer", sdp }
 2. DO: getWebSockets("bob") -> relay dm-offer
 3. Bob: acepta, responde { dm-signal, to: alice, kind: "answer", sdp }
 4. DO: relay
 5. ICE candidates relayed igual
 6. DataChannel "dm" establecido -> typing + chat real-time P2P
 7. Chat DM: P2P delivery + REST POST para persistir
```

### 4.2 Voice data channel

Al establecer la peer connection de voz, crear `createDataChannel("chat")`.
Typing y mensajes de la llamada van por ahi.

### 4.3 En lumen-voice (Rust)

```rust
// En VoiceSession, al crear RTCPeerConnection:
let dc = pc.create_data_channel("chat", None).await?;
let mut dc_events = dc.receiver().events();
// spawn task: dc_events -> mpsc -> host (VoiceEvent::DataChannelMessage)

// En PeerHandler, on_data_channel:
if channel.label() == "chat" {
    // enrutar mensajes al host
}
```

---

## 5. Seguridad

### 5.1 Rate limiting

REST (Worker): Cache API con `cf-connecting-ip` + ruta como key.
WS (DO): contadores en attachment.

| Ruta | Limite | Ventana | Scope |
|---|---|---|---|
| register | 3 | 1h | IP |
| login | 10 | 5min | IP |
| oauth callback | 10 | 5min | IP |
| POST servers | 5 | 1h | user |
| POST channels | 10 | 1h | user |
| POST friends/requests | 10 | 1h | user |
| PATCH/DELETE messages | 30 | 1min | user |
| POST dms | 10 | 1h | user |
| POST messages REST (fallback) | 30 | 30s | user |
| General REST | 100 | 1min | IP |
| WS chat | 10 | 10s | socket |
| WS typing | 3 | 5s | socket |
| WS voice-join | 5 | 1min | socket |
| WS dm-signal | 20 | 1min | socket |

Respuesta: `429 { error: "rate_limited" }` + header `Retry-After`.

### 5.2 CORS

```typescript
const ALLOWED_ORIGINS = new Set([
  "http://localhost:8787",       // dev
  "https://app.lumen.chat",      // prod (ajustar dominio real)
]);

function corsHeaders(request: Request): Record<string, string> {
  const origin = request.headers.get("origin");
  if (origin && ALLOWED_ORIGINS.has(origin)) {
    return {
      "access-control-allow-origin": origin,
      "access-control-allow-methods": "GET, POST, PUT, PATCH, DELETE, OPTIONS",
      "access-control-allow-headers": "authorization, content-type",
      "access-control-max-age": "86400",
    };
  }
  return {}; // sin CORS headers para origins no permitidos
}
```

Nota: el cliente nativo (Slint/Tauri/WebView) no envia Origin en requests
normales; los navegadores si. Para el WebView del cliente, registrar su
origin. Para requests sin Origin (curl, desktop native) -> permitir (son
nuestros clientes, autenticados por JWT).

### 5.3 JWT refresh

| Token | TTL | Storage |
|---|---|---|
| access | 1 hora | Cliente (memory) |
| refresh | 30 dias | Cliente (persistente) + D1 tabla |

```sql
CREATE TABLE refresh_tokens (
  token_hash TEXT PRIMARY KEY,     -- SHA-256 del token (no raw)
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  revoked_at TEXT
);
CREATE INDEX idx_refresh_user ON refresh_tokens(user_id);
```

Flujo:

```
POST /api/auth/refresh  { refreshToken }
  -> validar hash en D1, no expirado, no revoked
  -> firmar nuevo access token
  -> opcionalmente rotar refresh token

POST /api/auth/logout   { refreshToken }
  -> revoke (UPDATE revoked_at)

DELETE /api/auth/sessions  (logout all)
  -> UPDATE refresh_tokens SET revoked_at WHERE user_id
```

Access token en WS upgrade sigue funcionando (1h TTL, el upgrade valida
una vez al conectar; la sesion vive mientras el socket viva).

---

## 6. Schema D1 (target completo)

```sql
-- 0001: users, servers, server_members, channels, friendships, messages
-- 0002: dm_members, channels.kind += 'dm'
-- 0003 (NUEVO):
CREATE TABLE message_blocks (
  id TEXT PRIMARY KEY,
  channel_id TEXT NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  messages TEXT NOT NULL,          -- JSON array
  count INTEGER NOT NULL,
  first_at TEXT NOT NULL,
  last_at TEXT NOT NULL
);
CREATE INDEX idx_blocks_channel ON message_blocks(channel_id, last_at);

ALTER TABLE channels ADD COLUMN topic TEXT;
ALTER TABLE channels ADD COLUMN position INTEGER NOT NULL DEFAULT 0;

ALTER TABLE messages ADD COLUMN edited_at TEXT;
ALTER TABLE messages ADD COLUMN deleted_at TEXT;
ALTER TABLE messages ADD COLUMN reply_to TEXT REFERENCES messages(id);

ALTER TABLE servers ADD COLUMN icon TEXT;        -- R2 key
ALTER TABLE servers ADD COLUMN invite_regenerated_at TEXT;

ALTER TABLE users ADD COLUMN avatar TEXT;        -- R2 key
ALTER TABLE users ADD COLUMN password_version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE users ADD COLUMN deleted_at TEXT;    -- soft delete

CREATE TABLE refresh_tokens (...);

-- 0004 (OAuth):
ALTER TABLE users ADD COLUMN email TEXT;
ALTER TABLE users ADD COLUMN oauth_provider TEXT;
ALTER TABLE users ADD COLUMN oauth_id TEXT;
CREATE UNIQUE INDEX idx_users_oauth ON users(oauth_provider, oauth_id)
  WHERE oauth_provider IS NOT NULL;
CREATE TABLE oauth_states (
  state TEXT PRIMARY KEY,
  expires_at INTEGER NOT NULL
);

-- 0005 (Moderacion):
CREATE TABLE server_bans (
  server_id TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  reason TEXT,
  banned_by TEXT NOT NULL REFERENCES users(id),
  created_at TEXT NOT NULL,
  PRIMARY KEY (server_id, user_id)
);
CREATE TABLE blocks (
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  blocked_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL,
  PRIMARY KEY (user_id, blocked_id)
);
CREATE TABLE reports (
  id TEXT PRIMARY KEY,
  reporter_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  target_type TEXT NOT NULL,        -- 'message' | 'user' | 'server'
  target_id TEXT NOT NULL,
  reason TEXT,
  created_at TEXT NOT NULL
);
CREATE INDEX idx_reports_target ON reports(target_type, target_id);
```

---

## 7. R2 keys

```
avatars/<userId>.png
server-icons/<serverId>.png
attachments/<messageId>/<filename>
```

Serve via Worker con cache-control largo. Upload via PUT autenticado con
size limit (5 MB avatar, 25 MB attachment).

---

## 8. Permisos (modelo minimo viable)

Sin roles completos en v1; matriz simple:

| Operacion | Permiso |
|---|---|
| Editar server name/icon | owner |
| Borrar server | owner |
| Crear channel | owner |
| Editar/borrar channel | owner |
| Kick/ban member | owner |
| Transferir ownership | owner |
| Invite join | cualquier miembro con el codigo |
| Enviar mensaje | cualquier miembro |
| Editar/borrar mensaje | autor, o owner del server |
| Leave server | cualquier miembro |
| Borrar DM | cualquiera de los 2 |

Futuro (Fase 6): roles con permisos granularizados
(`manage_channels`, `manage_members`, `moderate_messages`, ...).

---

## 9. Deeplinks

### 9.1 Desktop nativo (Slint)

Registro de protocolo `lumen://` (documentado en Tauri y soportado por
winit/tauri en desktop; en Slint puro, el OS manda el URL al binario via
argv o D-Bus/Windows events — implementar hook en main.rs).

| Deeplink | Destino |
|---|---|
| `lumen://auth/callback?token=JWT` | Guardar token, cerrar ventana OAuth |
| `lumen://invite/CODE` | Abrir dialog "unirse a server" con CODE precargado |
| `lumen://server/:id/channel/:channelId` | Navegar directo al canal |
| `lumen://dm/:userId` | Abrir DM con usuario |

### 9.2 Web (si se hace web client futuro)

Same paths, via `navigator` / URL handling en el SPA.

### 9.3 OAuth redirect

El callback de OAuth redirige a:
- Desktop: `lumen://auth/callback?token=...`
- Web: `https://app.lumen.chat/auth/callback?token=...`

El Worker detecta el user-agent o un query param `client=desktop|web`.

---

## 10. Observabilidad

| Metrica | Como |
|---|---|
| Errores | `console.error` + estructura `{ err, route, userId }` |
| Request count | Analytics Engine (gratis, 400MB/dia) |
| DO request count | Dashboard Cloudflare |
| D1 usage | Dashboard Cloudflare |
| Latencia | `x-lumen-ms` header + log |
| Health | `GET /api/health` -> `{ status, d1: "ok", do: "ok", version }` |

```toml
# wrangler.toml addition
[analytics_engines]
bindings = [{ binding = "LUMEN_ANALYTICS", dataset = "lumen_requests" }]
```

```typescript
// en fetch handler
ctx.env.LUMEN_ANALYTICS.writeDataPoint({
  indexes: [url.pathname],
  doubles: [performance.now() - start],
  blobs: [request.method, String(status)]
});
```
