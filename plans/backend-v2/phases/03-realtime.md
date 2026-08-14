# Fase 3 — Real-time, presencia y chat eficiente

Prioridad: **ALTA** | Dependencias: Fase 2 | Duracion estimada: 5-7 dias

## Objetivo

El corazon de la app: presencia global, voice occupancy visible, chat
real-time con buffer 50x, paginacion, y P2P data channels. Esta fase
entrega las features que el usuario pidio explicitamente:
- Estado de amigos (online/offline real)
- Amigos de un server conectados
- Ver quienes estan en voice sin entrar al canal
- Escalar mensajes al maximo del free tier

## Archivos

| Archivo | Cambio |
|---|---|
| `src/do/PresenceHubDO.ts` | **NUEVO** — el componente central (orquestación, Hibernation) |
| `src/do/lib/buffer.ts` | **NUEVO** — lógica pura: buffer, umbral de flush, edit/delete en entries (unit-testable sin DO) |
| `src/do/lib/ws-rate-limit.ts` | **NUEVO** — lógica pura: ventanas deslizantes por tipo (unit-testable sin DO) |
| `src/do/lib/presence-utils.ts` | **NUEVO** — lógica pura: snapshot builders, dedup clientIds (unit-testable sin DO) |
| `src/index.ts` | Ruta `/api/presence` WS upgrade + query D1 + rutas de buffer |
| `src/db.ts` | Queries de servers+friends para el upgrade |
| `packages/protocol/src/index.ts` | Tipos Presence* + MessageBlock |
| `wrangler.toml` | Binding `LUMEN_PRESENCE_DO` |
| `src/env.d.ts` | Tipos del binding |
| `crates/lumen-voice/src/signaling.rs` | Data channels (DM) |
| `crates/lumen-voice/src/client.rs` | DataChannel en voice |
| `crates/lumen-core/src/state.rs` | Estado de presencia en cliente |
| `crates/lumen-core/src/api.rs` | Conexion presence WS |
| `apps/lumen-slint/src/controller.rs` | Manejo de eventos presence |
| `apps/lumen-slint/src/model.rs` | Modelo: online members, voice occupancy |
| `apps/lumen-slint/ui/*.slint` | UI: lista de online, indicador de voz |
| `test/smoke-ws.mjs` | Tests presence + chat RT |

## Tareas

### 3.1 wrangler.toml + env.d.ts

```toml
[durable_objects]
bindings = [
  { name = "LUMEN_CHANNEL_DO", class_name = "LumenChannelDO" },
  { name = "LUMEN_PRESENCE_DO", class_name = "PresenceHubDO" },
]

[[migrations]]
tag = "v2"
new_sqlite_classes = ["PresenceHubDO"]
```

```typescript
// env.d.ts
LUMEN_PRESENCE_DO: DurableObjectNamespace;
```

### 3.2 PresenceHubDO.ts

```typescript
import type { PresenceClientMessage, PresenceServerMessage } from "@lumen/protocol";

interface PresenceAttachment {
  userId: string;
  username: string;
  status: "online" | "idle" | "dnd";
  servers: string[];
  friends: string[];
  voiceChannelId: string | null;
  voiceServerId: string | null;
  // rate limiting
  msgWindowStart: number;
  msgCount: number;
  typingWindowStart: number;
  typingCount: number;
}

const CHAT_LIMIT = 10;          // mensajes por ventana
const CHAT_WINDOW = 10_000;     // 10s
const TYPING_LIMIT = 3;
const TYPING_WINDOW = 5_000;    // 5s
const FLUSH_THRESHOLD = 50;     // mensajes por block
const FLUSH_INTERVAL = 5 * 60_000; // 5 min

export class PresenceHubDO {
  private state: DurableObjectState;
  private env: Env;

  constructor(state: DurableObjectState, env: Env) {
    this.state = state;
    this.env = env;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);

    // Consulta de buffer para paginacion (Worker -> DO)
    if (url.pathname.startsWith("/buffer/")) {
      const channelId = url.pathname.slice("/buffer/".length);
      const buf = await this.state.storage.get<BufferedMessage[]>(`buf:${channelId}`);
      return new Response(JSON.stringify(buf ?? []), {
        headers: { "content-type": "application/json" },
      });
    }

    // WS upgrade
    const userId = url.searchParams.get("userId");
    const username = url.searchParams.get("username");
    const servers = (url.searchParams.get("servers") ?? "").split(",").filter(Boolean);
    const friends = (url.searchParams.get("friends") ?? "").split(",").filter(Boolean);
    if (!userId || !username) {
      return new Response(JSON.stringify({ error: "missing_user" }), { status: 400 });
    }

    const pair = new WebSocketPair();
    const server = pair[1];
    this.state.acceptWebSocket(server, [userId, ...servers.map(s => `s:${s}`)]);
    server.serializeAttachment({
      userId, username, status: "online", servers, friends,
      voiceChannelId: null, voiceServerId: null,
      msgWindowStart: 0, msgCount: 0, typingWindowStart: 0, typingCount: 0,
    } satisfies PresenceAttachment);

    // Notificar a amigos online + armar snapshot
    this.notifyFriendsAndBuildSnapshot(server, { userId, username, servers, friends })
      .catch((e) => console.error("presence connect:", e));

    return new Response(null, { status: 101, webSocket: pair[0] });
  }

  private async notifyFriendsAndBuildSnapshot(
    ws: WebSocket, att: Pick<PresenceAttachment, "userId" | "username" | "servers" | "friends">
  ) {
    const onlineFriends: { userId: string; username: string; status: string }[] = [];
    for (const friendId of att.friends) {
      const sockets = this.state.getWebSockets(friendId);
      if (sockets.length > 0) {
        const friendAtt = sockets[0].deserializeAttachment() as PresenceAttachment;
        onlineFriends.push({ userId: friendId, username: friendAtt.username, status: friendAtt.status });
        // notificar al amigo
        sockets[0].send(JSON.stringify({
          type: "friend-online", userId: att.userId, username: att.username,
        } satisfies PresenceServerMessage));
      }
    }

    // Snapshot por server
    const servers: PresenceServerMessage["servers"] = [];
    for (const serverId of att.servers) {
      const members = this.state.getWebSockets(`s:${serverId}`);
      const onlineMembers: { userId: string; username: string }[] = [];
      const voiceChannels: { channelId: string; peers: { userId: string; username: string }[] }[] = [];
      const byVoice = new Map<string, { userId: string; username: string }[]>();
      for (const s of members) {
        if (s.readyState !== 1) continue;
        const a = s.deserializeAttachment() as PresenceAttachment;
        onlineMembers.push({ userId: a.userId, username: a.username });
        if (a.voiceChannelId) {
          const list = byVoice.get(a.voiceChannelId) ?? [];
          list.push({ userId: a.userId, username: a.username });
          byVoice.set(a.voiceChannelId, list);
        }
      }
      for (const [channelId, peers] of byVoice) {
        voiceChannels.push({ channelId, peers });
      }
      servers.push({ serverId, onlineMembers, voiceChannels });
    }

    ws.send(JSON.stringify({ type: "ready", onlineFriends, servers } satisfies PresenceServerMessage));
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    let msg: PresenceClientMessage;
    try {
      msg = JSON.parse(typeof message === "string" ? message : new TextDecoder().decode(message));
    } catch {
      this.sendError(ws, "bad_message", "invalid JSON");
      return;
    }
    const att = ws.deserializeAttachment() as PresenceAttachment | null;
    if (!att) return;

    switch (msg.type) {
      case "status": {
        att.status = msg.status;
        ws.serializeAttachment(att);
        // broadcast a amigos y servers
        this.broadcastToFriends(att, { type: "friend-status", userId: att.userId, status: msg.status });
        break;
      }

      case "voice-join": {
        // rate limit
        if (!this.checkRate(att, "voice", 5, 60_000, ws)) return;
        if (!att.servers.includes(msg.serverId)) {
          this.sendError(ws, "forbidden", "not a member");
          return;
        }
        att.voiceChannelId = msg.channelId;
        att.voiceServerId = msg.serverId;
        ws.serializeAttachment(att);
        this.broadcastVoiceUpdate(msg.serverId, ws);
        break;
      }

      case "voice-leave": {
        att.voiceChannelId = null;
        att.voiceServerId = null;
        ws.serializeAttachment(att);
        // broadcast a server del que salio
        if (att.voiceServerId) this.broadcastVoiceUpdate(att.voiceServerId, ws);
        break;
      }

      case "chat": {
        if (!this.checkRate(att, "chat", CHAT_LIMIT, CHAT_WINDOW, ws)) return;
        if (!att.servers.includes(msg.serverId)) {
          this.sendError(ws, "forbidden", "not a member");
          return;
        }
        // Re-validacion de membership (cierra hueco de kick/ban en WS
        // activos, R4 del review): 1 D1 read por mensaje = ~50k/dia con
        // 500 users = 1% del limite de 5M. Coste aceptado.
        const member = await this.env.LUMEN_D1.prepare(
          "SELECT 1 FROM server_members WHERE server_id = ? AND user_id = ?"
        ).bind(msg.serverId, att.userId).first().catch(() => null);
        if (!member) {
          // miembro removido/baneado -> cerrar acceso
          this.sendError(ws, "forbidden", "no longer a member");
          ws.close(4403, "kicked");
          return;
        }
        const message = {
          id: crypto.randomUUID(),
          authorId: att.userId,
          authorName: att.username,
          content: msg.content.slice(0, 2000),
          createdAt: new Date().toISOString(),
        };
        // buffer
        const key = `buf:${msg.channelId}`;
        const buf = (await this.state.storage.get<typeof message[]>(key)) ?? [];
        buf.push(message);
        await this.state.storage.put(key, buf);
        // broadcast a suscritos
        this.broadcastToChannel(msg.channelId, { type: "chat", channelId: msg.channelId, message }, ws);
        // ACK
        ws.send(JSON.stringify({
          type: "chat-ack", clientId: msg.clientId, messageId: message.id, createdAt: message.createdAt,
        } satisfies PresenceServerMessage));
        // flush?
        if (buf.length >= FLUSH_THRESHOLD) {
          await this.flushChannel(msg.channelId, buf);
        } else if (!(await this.state.storage.getAlarm())) {
          await this.state.storage.setAlarm(Date.now() + FLUSH_INTERVAL);
        }
        break;
      }

      case "typing": {
        if (!this.checkRate(att, "typing", TYPING_LIMIT, TYPING_WINDOW, ws)) return;
        this.broadcastToChannel(msg.channelId, {
          type: "typing", channelId: msg.channelId, userId: att.userId,
        }, ws);
        break;
      }

      case "chat-edit": {
        if (!att.servers.includes(msg.serverId)) { this.sendError(ws, "forbidden", "not a member"); return; }
        const ok = await bufferEdit(this.state, msg.channelId, msg.messageId, msg.content); // do/lib/buffer.ts
        if (!ok) { this.sendError(ws, "bad_message", "message not found"); return; }
        ws.send(JSON.stringify({ type: "chat-edit-ack", clientId: msg.clientId, messageId: msg.messageId } satisfies PresenceServerMessage));
        this.broadcastToChannel(msg.channelId, {
          type: "chat-edited", channelId: msg.channelId,
          message: { id: msg.messageId, content: msg.content, editedAt: new Date().toISOString() },
        }, ws);
        break;
      }

      case "chat-delete": {
        if (!att.servers.includes(msg.serverId)) { this.sendError(ws, "forbidden", "not a member"); return; }
        const ok = await bufferDelete(this.state, msg.channelId, msg.messageId); // do/lib/buffer.ts
        if (!ok) { this.sendError(ws, "bad_message", "message not found"); return; }
        ws.send(JSON.stringify({ type: "chat-delete-ack", clientId: msg.clientId, messageId: msg.messageId } satisfies PresenceServerMessage));
        this.broadcastToChannel(msg.channelId, {
          type: "chat-deleted", channelId: msg.channelId, messageId: msg.messageId,
        }, ws);
        break;
      }

      case "subscribe": {
        this.state.acceptWebSocket(ws, [`c:${msg.channelId}`]);
        break;
      }

      case "unsubscribe": {
        // no hay API para quitar tags individuales en Hibernation;
        // documentar: re-conectar o tolerar broadcasts extra (minimo)
        break;
      }

      case "dm-signal": {
        const target = this.state.getWebSockets(msg.to)[0];
        if (!target || target.readyState !== 1) {
          this.sendError(ws, "offline", "user offline");
          return;
        }
        const relay: PresenceServerMessage =
          msg.kind === "offer" ? { type: "dm-offer", from: att.userId, sdp: msg.sdp! }
          : msg.kind === "answer" ? { type: "dm-answer", from: att.userId, sdp: msg.sdp! }
          : { type: "dm-ice", from: att.userId, candidate: msg.candidate! };
        target.send(JSON.stringify(relay));
        break;
      }

      case "ping": {
        ws.send(JSON.stringify({ type: "pong" } satisfies PresenceServerMessage));
        break;
      }

      default: this.sendError(ws, "bad_message", "unknown type");
    }
  }

  async webSocketClose(ws: WebSocket): Promise<void> {
    const att = ws.deserializeAttachment() as PresenceAttachment | null;
    if (!att) return;

    // notificar servers
    for (const serverId of att.servers) {
      this.broadcastToTag(`s:${serverId}`,
        { type: "member-offline", serverId, userId: att.userId },
        ws);
      if (att.voiceChannelId) this.broadcastVoiceUpdate(serverId, ws);
    }
    // notificar amigos
    this.broadcastToFriends(att, { type: "friend-offline", userId: att.userId });

    // last_seen en D1 (1 write por disconnect)
    await this.env.LUMEN_D1.prepare(
      "UPDATE users SET last_seen = ? WHERE id = ?"
    ).bind(new Date().toISOString(), att.userId).run().catch(() => {});
  }

  async alarm(): Promise<void> {
    // flush todos los buffers
    const entries = await this.state.storage.list({ prefix: "buf:" });
    for (const [key, buf] of entries) {
      if (buf.length > 0) {
        await this.flushChannel(key.slice(4), buf);
      }
    }
  }

  private async flushChannel(channelId: string, messages: BufferedMessage[]): Promise<void> {
    await this.env.LUMEN_D1.prepare(
      `INSERT INTO message_blocks (id, channel_id, messages, count, first_at, last_at)
       VALUES (?, ?, ?, ?, ?, ?)`
    ).bind(
      crypto.randomUUID(), channelId,
      JSON.stringify(messages), messages.length,
      messages[0].createdAt, messages[messages.length - 1].createdAt,
    ).run().catch((e) => console.error("flush:", e));
    await this.state.storage.delete(`buf:${channelId}`);
  }

  private broadcastToChannel(channelId: string, msg: PresenceServerMessage, except?: WebSocket): void {
    this.broadcastToTag(`c:${channelId}`, msg, except);
  }

  private broadcastToTag(tag: string, msg: PresenceServerMessage, except?: WebSocket): void {
    const payload = JSON.stringify(msg);
    for (const s of this.state.getWebSockets(tag)) {
      if (s.readyState === 1 && s !== except) s.send(payload);
    }
  }

  private broadcastToFriends(att: PresenceAttachment, msg: PresenceServerMessage): void {
    const payload = JSON.stringify(msg);
    for (const friendId of att.friends) {
      for (const s of this.state.getWebSockets(friendId)) {
        if (s.readyState === 1) s.send(payload);
      }
    }
  }

  private broadcastVoiceUpdate(serverId: string, except: WebSocket): void {
    // recomputa occupancy por server y broadcast
    const members = this.state.getWebSockets(`s:${serverId}`);
    const byVoice = new Map<string, { userId: string; username: string }[]>();
    for (const s of members) {
      if (s.readyState !== 1) continue;
      const a = s.deserializeAttachment() as PresenceAttachment;
      if (a.voiceChannelId) {
        const list = byVoice.get(a.voiceChannelId) ?? [];
        list.push({ userId: a.userId, username: a.username });
        byVoice.set(a.voiceChannelId, list);
      }
    }
    for (const [channelId, peers] of byVoice) {
      this.broadcastToTag(`s:${serverId}`,
        { type: "voice-update", serverId, channelId, peers }, except);
    }
  }

  private checkRate(
    att: PresenceAttachment, kind: "chat" | "typing" | "voice",
    limit: number, windowMs: number, ws: WebSocket
  ): boolean {
    const now = Date.now();
    const key = kind === "chat" ? "msgCount" : kind === "typing" ? "typingCount" : "msgCount";
    const windowKey = kind === "chat" ? "msgWindowStart" : kind === "typing" ? "typingWindowStart" : "msgWindowStart";
    if (now - att[windowKey] > windowMs) {
      att[key] = 1;
      att[windowKey] = now;
      ws.serializeAttachment(att);
      return true;
    }
    att[key]++;
    ws.serializeAttachment(att);
    if (att[key] > limit) {
      this.sendError(ws, "rate_limited", "slow down");
      return false;
    }
    return true;
  }

  private sendError(ws: WebSocket, code: string, message: string): void {
    if (ws.readyState !== 1) return;
    ws.send(JSON.stringify({ type: "error", code, message } satisfies PresenceServerMessage));
  }
}
```

### 3.3 Rutas Worker nuevas

```typescript
// WS upgrade presence
router.get("/api/presence", true, async (ctx) => {
  // 1 query optimizada: servers + friends del user
  const data = await db.getUserPresenceContext(ctx.env.LUMEN_D1, ctx.user.id);
  const url = new URL(ctx.request.url);
  url.searchParams.set("userId", ctx.user.id);
  url.searchParams.set("username", ctx.user.username);
  url.searchParams.set("servers", data.servers.join(","));
  url.searchParams.set("friends", data.friends.join(","));
  const id = ctx.env.LUMEN_PRESENCE_DO.idFromName("hub");
  const stub = ctx.env.LUMEN_PRESENCE_DO.get(id);
  return await stub.fetch(new Request(url.toString(), ctx.request));
});
```typescript
// Mensajes con buffer: GET combina blocks + buffer
router.get("/api/channels/:id/messages", true, async (ctx, params) => {
  const before = ctx.url.searchParams.get("before");   // "<lastAt>,<id>" cursor compuesto
  const [beforeAt, beforeId] = before?.split(",") ?? [];
  // blocks de D1 (1 read)
  const blocks = await db.getMessageBlocks(ctx.env.LUMEN_D1, params.id!, beforeAt, beforeId);
  let msgs: TextMessage[] = blocks.flatMap(b => JSON.parse(b.messages));
  // buffer pendiente (solo si es la primera pagina)
  if (!before) {
    const hubId = ctx.env.LUMEN_PRESENCE_DO.idFromName("hub");
    const hub = ctx.env.LUMEN_PRESENCE_DO.get(hubId);
    const res = await hub.fetch(`https://do/buffer/${params.id!}`);
    const buffered = await res.json() as TextMessage[];
    msgs = [...msgs, ...buffered];
    msgs.sort((a, b) => a.createdAt.localeCompare(b.createdAt) || a.id.localeCompare(b.id));
  }
  return json(msgs.slice(-100));
});
```

Nota: la validacion de acceso al canal se mantiene (canAccessChannel).

### 3.4 protocol: tipos nuevos

```typescript
export type PresenceStatus = "online" | "idle" | "dnd";

export type PresenceClientMessage = ...;  // ver ARCHITECTURE.md §2.2
export type PresenceServerMessage = ...;  // ver ARCHITECTURE.md §2.3

export interface MessageBlock { id; channelId; count; firstAt; lastAt; }
export interface BufferedMessage {
  id: string; authorId: string; authorName: string;
  content: string; createdAt: string;
}
```

### 3.5 Cliente (lumen-voice): DataChannels

```rust
// crates/lumen-voice/src/client.rs — en VoiceSession::connect_peer:
let dc = pc.create_data_channel("chat", None).await?;
let mut events = dc.receiver().events();
let sender = session_tx.clone();
task::spawn(async move {
    while let Some(ev) = events.next().await {
        if let DataChannelEvent::Message(m) = ev {
            let _ = sender.send(VoiceEvent::DataChannelMessage(m.data.to_vec())).await;
        }
    }
});
```

```rust
// VoiceEvent nuevo:
pub enum VoiceEvent {
    // ... existing ...
    DataChannelMessage(Vec<u8>),
}
```

DM signaling reutiliza los mismos primitivos WebRTC, con un modo
"data-only" (sin audio): `VoiceClient::join_dm(peer)` establece peer
connection sin tracks.

### 3.6 Cliente (lumen-core): estado de presencia

```rust
// state.rs adiciones
pub struct PresenceState {
    pub online_friends: RwLock<HashMap<String, FriendPresence>>,
    pub servers: RwLock<HashMap<String, ServerPresence>>, // onlineMembers + voiceChannels
    pub voice_occupancy: RwLock<HashMap<String, Vec<String>>>, // channelId -> userIds
}
```

EventBus eventos nuevos:
```rust
pub enum CoreEvent {
    // ... existing ...
    FriendOnline(String),        // userId
    FriendOffline(String),
    VoiceOccupancyChanged(String), // channelId
    MemberOnline(String, String),  // serverId, userId
    RealtimeMessage(TextMessage),  // de WS
    ChatAck(String, String),       // clientId, messageId
}
```

### 3.7 UI (Slint)

- Sidebar server: lista de miembros online con dot verde
- Voice channels: badge con numero de peers (sin entrar)
- Friends tab: estado en tiempo real
- Mensajes: indicador "editado", placeholder "mensaje eliminado"
- Typing indicator en el canal activo

## Aceptacion

- [ ] `pnpm smoke` pasa con tests presence (connect, friend-online,
      voice-update, chat RT + ACK, flush, paginacion)
- [ ] Unit tests de `do/lib/*` sin DO runtime: buffer (umbral/alarm/edit/delete),
      ws-rate-limit (ventanas), presence-utils (snapshot/dedup)
- [ ] Edit/delete via WS: mensaje en buffer y en block flusheado (rewrite)
- [ ] Dos clientes: B ve a A online en el server al conectar A
- [ ] B ve voice occupancy de un canal sin entrar (badge peers)
- [ ] Chat: mensaje llega instantaneo via WS; ACK con messageId
- [ ] 50+ mensajes -> 1 fila en message_blocks (verificar en D1)
- [ ] Paginacion: scroll up carga block anterior (1 D1 read)
- [ ] Rate limit chat: 11 mensajes en 10s -> rate_limited
- [ ] P2P DM: typing entre 2 peers no toca el servidor
- [ ] Budget: < 5% DO, < 3% D1 writes con 500 users (medir)
- [ ] Hibernation: DO sin storage reads para presencia (tags only)

## Riesgos

| Riesgo | Mitigacion |
|---|---|
| `unsubscribe` sin API de tags | Tolerar broadcasts extra a suscriptores; costo ~0 (outgoing gratis). O re-conectar socket |
| Broadcast recomputa occupancy O(members) | Necesario para exactitud; members por server tipicamente < 100 |
| Buffer en storage + alarm juntos | Durable: crash antes de flush = mensajes sobreviven en storage; alarm re-programada en proximo mensaje |
| Chat message perdido (crash entre storage.put y ACK) | Cliente retransmite si no recibe ACK en 3s; dedup por clientId en DO (guardar ultimos clientIds en attachment) |
| Flush falla (D1 error) | catch -> retry en proxima alarm; buffer intacto |
| Tags: subscribe agrega tag, socket attachment no cambia | OK: tags y attachment son independientes |

## Presupuesto de esta fase (500 users)

| Recurso | Consumo | % limite |
|---|---|---|
| DO requests | ~4,300/dia | 4.3% |
| D1 writes | ~1,500/dia | 1.5% |
| D1 reads | ~2,000/dia | 0.04% |
| Worker requests | ~8,500/dia | 8.5% |
