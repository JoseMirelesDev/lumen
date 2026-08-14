# Lumen signaling protocol — v1

Status: **contract** (cross-slice interface between `@lumen/backend` and `@lumen/desktop`; both import the types from `@lumen/protocol`).

## 1. Transport

- All real-time signaling runs over a single WebSocket per (client, channel) pair.
- URL: `wss://<worker>/api/ws/:channelId`, opened with `Authorization: Bearer <token>` (non-browser clients) or `?token=<jwt>` (browser/WebView WebSocket cannot set headers). Token is HMAC-signed with a 7-day expiry; the query form is accepted only on the WS upgrade path.
- Messages are JSON text frames. One message per frame; no batching in v1.
- The worker upgrades and forwards the socket to `LumenChannelDO` (instance name `lumen-<channelId>`).
- Max 4 peers per voice channel (enforced by the DO: `error "channel_full"` on the 5th join).

## 2. Message flow

```
client ──join──▶ DO
DO      ──joined──▶ client        (your peerId + snapshot of current peers)
DO      ──peer-joined──▶ everyone (a new peer arrived, after it got `joined`)
client ⇄ DO ⇄ peer: offer / answer / ice-candidate (relayed verbatim, `from`/`to` are peerIds)
client ──presence──▶ DO ──presence──▶ everyone (status changes)
client ──ping──▶ DO ──pong──▶ client
DO      ──peer-left──▶ everyone   (peer's WS closed)
```

### Client → DO (`ClientMessage`)

| type | fields | notes |
|---|---|---|
| `join` | `channelId`, `userId` | first message on the socket; DO rejects anything else before it |
| `offer` | `to`, `sdp` | relayed to peer `to` as `{type:"offer", from, sdp}` |
| `answer` | `to`, `sdp` | relayed as `{type:"answer", from, sdp}` |
| `ice-candidate` | `to`, `candidate` | relayed as `{type:"ice-candidate", from, candidate}` |
| `presence` | `status` | `online \| idle \| offline`; broadcast to all peers |
| `ping` | — | app-level liveness probe; DO answers `pong` without touching storage |

### DO → Client (`ServerMessage`)

| type | fields | notes |
|---|---|---|
| `joined` | `peerId`, `peers` | sent to the joining socket only; `peers` = existing members (mesh bootstrap) |
| `peer-joined` | `peer` | `{peerId, userId}` broadcast after `joined` |
| `peer-left` | `peerId` | broadcast on WS close/error |
| `offer` | `from`, `sdp` | `from` = sender's peerId |
| `answer` | `from`, `sdp` | |
| `ice-candidate` | `from`, `candidate` | |
| `presence` | `userId`, `status` | |
| `pong` | — | answer to `ping` |
| `error` | `code`, `message` | `channel_full`, `bad_message`, `not_joined`, `unauthorized` |

## 3. Deviations from the original brief spec — rationale

| change | why |
|---|---|
| added `joined` + peer list | a mesh cannot bootstrap without knowing who is already in the channel; the brief spec only told late joiners "someone arrived", never "here is everyone" |
| added `from` on relays | peers need to attribute offers/candidates to a peerId, and peerIds are DO-assigned |
| added `peer-joined` payload as object | carries both peerId and userId so the client can render presence without an extra lookup |
| added `error` + `ping`/`pong` | protocol failures need an explicit channel; app-level ping is optional but cheap (20:1 against DO budget) |
| presence scoped to channel | global (cross-server) presence would need a per-user DO; v1 derives "online" from `last_seen` (updated on login) + live channel membership. Documented in ADR 0001 "Presence scoping". |

## 4. REST API (same origin as WS)

All JSON; auth via `Authorization: Bearer <token>` (HMAC-signed JWT, see ADR 0001 "Auth").

| method | path | body | returns |
|---|---|---|---|
| POST | `/api/auth/register` | `{username, password}` | `201 {token, user}` |
| POST | `/api/auth/login` | `{username, password}` | `200 {token, user}` |
| GET | `/api/me` | — | `{user}` |
| POST | `/api/servers` | `{name}` | `201 {server, channels}` (creates `general` text + `General` voice) |
| GET | `/api/servers` | — | `[ServerWithChannels]` (member of) |
| POST | `/api/servers/join` | `{inviteCode}` | `200 {server, channels}` |
| GET | `/api/servers/:id` | — | `{server, channels, members}` (`members: [{id, username}]`) |
| POST | `/api/servers/:id/channels` | `{name, kind}` | `201 {channel}` (owner only) |
| POST | `/api/channels/:id/messages` | `{content}` | `201 {message}` |
| GET | `/api/channels/:id/messages?limit=50` | — | `[message]` |
| GET | `/api/friends` | — | `{friends: [FriendInfo], pending: [FriendshipRequest]}` |
| POST | `/api/friends/requests` | `{username}` | `201 {request}` |
| POST | `/api/friends/requests/:id/accept` | — | `200 {friend: User}` |
| GET | `/api/realtime/config` | — | `{iceServers}` (Cloudflare Realtime ephemeral creds; STUN-only fallback) |
| GET | `/api/ws/:channelId` | WS upgrade | socket into `LumenChannelDO` |

Errors: `{error: string}` with 4xx/5xx status. Validation errors use `422`.

## 5. Budget posture

- Every REST mutation touches D1 at most once per request.
- `typing` indicators, presence, and peer state: DO memory only — never D1.
- DO hibernates after every message (peer map persisted to `state.storage` key `peers`).
- No app-level timers in the DO. Liveness = WS close events; a crashed tab is detected by the edge and surfaces as `peer-left` (may lag by the edge's TCP timeout).

---

## 6. Presence v2 (protocolo de presencia + chat real-time)

**Status: contract** (implementado en Fase 3; spec detallada en
`plans/backend-v2/protocol/presence-v2.md`). Complementa el v1 (que no cambia):
un socket adicional por sesión, abierto al login, cerrado al logout.

- URL: `wss://<worker>/api/presence?token=<jwt>` → `PresenceHubDO` (singleton
  `"hub"`, ADR-003). El Worker resuelve servers/friends del usuario desde D1
  (el cliente solo envía el token).
- JSON text frames; tipos `PresenceClientMessage` / `PresenceServerMessage`
  en `@lumen/protocol` (presence-v2).

### Mensajes cliente → hub

| type | fields | notas |
|---|---|---|
| `status` | `status` (online/idle/dnd) | broadcast a amigos |
| `voice-join` | `channelId`, `serverId` | re-valida membership (R4); broadcast voice-update |
| `voice-leave` | — | |
| `chat` | `channelId`, `serverId`, `content`, `clientId`, `replyTo?`, `attachmentUrl?` | buffer durable + ACK + broadcast a suscritos; dedup por clientId |
| `chat-edit` | `channelId`, `serverId`, `messageId`, `content`, `clientId` | author-only (ADR-0010); buffer o block (rewrite) |
| `chat-delete` | `channelId`, `serverId`, `messageId`, `clientId` | idem |
| `typing` | `channelId`, `serverId` | rate 3/5s, best-effort |
| `subscribe`/`unsubscribe` | `channelId` | suscripción por attachment (tags inmutables post-accept, workerd#958) |
| `reaction-toggle` | `channelId`, `serverId`, `messageId`, `emoji` | toggle + broadcast |
| `dm-signal` | `to`, `kind` (offer/answer/ice), `sdp?`, `candidate?` | relay P2P para DataChannels (ADR-006) |
| `ping` | — | `pong` |

### Mensajes hub → cliente

| type | fields |
|---|---|
| `ready` | `onlineFriends[]`, `servers[]` (onlineMembers + voiceChannels con peers) |
| `friend-online`/`friend-offline`/`friend-status` | `userId`, … |
| `member-online`/`member-offline` | `serverId`, `userId` |
| `voice-update` | `serverId`, `channelId`, `peers[]` (occupancy sin entrar al canal) |
| `typing` | `channelId`, `userId` |
| `chat` | `channelId`, `message` (BufferedMessage) |
| `chat-ack` | `clientId`, `messageId`, `createdAt` |
| `chat-edit-ack`/`chat-delete-ack` | `clientId`, `messageId` |
| `chat-edited`/`chat-deleted` | broadcast de invalidación |
| `chat-error` | `clientId`, `code` |
| `reaction` | `channelId`, `messageId`, `emoji`, `userId`, `added` |
| `dm-offer`/`dm-answer`/`dm-ice` | `from`, … |
| `pong` / `error` | — |

### Persistencia (ADR-0004)

Los mensajes se acumulan en el buffer del hub (`state.storage`, clave
`buf:<channelId>`) y se flushean a `message_blocks` (50 por fila o alarm de
5 min). El REST `GET /api/channels/:id/messages` es read-only: combina el
block más reciente + el buffer pendiente, con paginación por cursor compuesto
`before=<lastAt>,<id>` (1 read por página). **Todas las mutaciones de
mensajes (send/edit/delete) van por el WS** (ADR-0010); el REST de escritura
de mensajes fue eliminado en Fase 3.

### REST (cambios sobre la tabla v1)

- `POST /api/channels/:id/messages`, `PATCH/DELETE /api/messages/:id`:
  **eliminados** (Fase 3) — usar el WS.
- `GET /api/channels/:id/messages?before=<cursor>`: paginado (blocks + buffer).
- `PUT /api/messages/:id/reactions/:emoji` (toggle) y
  `GET /api/channels/:id/messages/reactions?messageIds=…` (agregado).
- `PUT /api/uploads?filename=…` → `{url}` (R2 attachment, 25 MB).
- `GET /api/presence`: WS upgrade (auth por token).
