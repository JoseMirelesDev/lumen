# Protocolo de presencia — v2 (borrador)

Status: **PROPUESTO** — complementa `docs/protocol.md` v1 (que no cambia;
el protocolo voice v1 sigue vigente para ChannelDO).

## Transporte

- WS adicional: `wss://<worker>/api/presence?token=<jwt>`
- Un socket por sesion de usuario, abierto al login, cerrado al logout/cierre
- JSON text frames, un mensaje por frame
- El socket presence es INDEPENDIENTE del socket voice del ChannelDO

## Flujo

```
client ──(connect)──▶ PresenceHubDO     upgrade con userId, username, servers, friends
DO      ──ready──▶ client              snapshot: onlineFriends + servers (onlineMembers + voiceChannels)
DO      ──friend-online──▶ amigos       cuando alguien en su lista conecta
DO      ──friend-offline──▶ amigos      cuando desconecta
client ──status──▶ DO ──friend-status──▶ amigos
client ──voice-join──▶ DO ──voice-update──▶ miembros del server
client ──voice-leave──▶ DO ──voice-update──▶ miembros del server
client ──subscribe──▶ DO               empieza a recibir chat RT del canal
client ──chat──▶ DO ──chat──▶ suscritos + chat-ack──▶ sender
DO      ──flush──▶ D1 (packed block)   >= 50 msgs o cada 5 min
client ──typing──▶ DO ──typing──▶ suscritos
client ⇄ DO ⇄ peer: dm-signal (offer/answer/ice)   relay para P2P DM data channel
client ──ping──▶ DO ──pong──▶ client
DO      ──member-online/offline──▶ miembros del server
```

## Mensajes (mismo shape que ARCHITECTURE.md §2.2-2.3)

### ClientMessage

| type | fields | notas |
|---|---|---|
| `ready` | — | pedir snapshot (opcional; el DO lo envia solo) |
| `status` | `status` | online/idle/dnd |
| `voice-join` | `channelId`, `serverId` | se valida membership |
| `voice-leave` | — | |
| `chat` | `channelId`, `serverId`, `content`, `clientId` | clientId para dedup/ACK |
| `chat-edit` | `channelId`, `serverId`, `messageId`, `content`, `clientId` | ADR-0010: mutación por el DO |
| `chat-delete` | `channelId`, `serverId`, `messageId`, `clientId` | ADR-0010 |
| `typing` | `channelId`, `serverId` | rate 3/5s |
| `subscribe` | `channelId` | tag c:channelId; el DO responde `subscribe-ack` |
| `unsubscribe` | `channelId` | best-effort |
| `dm-signal` | `to`, `kind` (offer/answer/ice), `sdp?`, `candidate?` | relay P2P |
| `ping` | — | |

### ServerMessage

| type | fields | notas |
|---|---|---|
| `ready` | `onlineFriends[]`, `servers[]` | snapshot inicial |
| `friend-online` | `userId`, `username` | |
| `friend-offline` | `userId` | |
| `friend-status` | `userId`, `status` | |
| `voice-update` | `serverId`, `channelId`, `peers[]` | occupancy actual |
| `member-online` | `serverId`, `userId`, `username` | |
| `member-offline` | `serverId`, `userId` | |
| `chat` | `channelId`, `message` | mensaje real-time |
| `chat-ack` | `clientId`, `messageId`, `createdAt` | |
| `chat-edit-ack` | `clientId`, `messageId` | |
| `chat-delete-ack` | `clientId`, `messageId` | |
| `chat-edited` | `channelId`, `message` (id/content/editedAt) | broadcast a suscritos |
| `chat-deleted` | `channelId`, `messageId` | broadcast a suscritos |
| `chat-error` | `clientId`, `code` | |
| `dm-offer` | `from`, `sdp` | |
| `dm-answer` | `from`, `sdp` | |
| `dm-ice` | `from`, `candidate` | |
| `pong` | — | |
| `subscribe-ack` | `channelId` | confirmación de suscripción — el cliente solo confía en recibir `chat` del canal después de este ack (el orden entre sockets no está garantizado; sin el ack un chat puede ser filtrado por broadcast) |
| `error` | `code`, `message` | forbidden, rate_limited, offline, bad_message |

## Compatibilidad

- El protocolo v1 (ChannelDO voice) NO cambia
- Un cliente que solo usa voice v1 funciona sin el socket presence
  (pierde presencia global, pero voice intacto)
- El cliente v2 usa ambos sockets

## Budget

- Presence WS: 1 full DO request por conexion
- Cada mensaje client->DO: 1/20 request
- Broadcasts: gratis
- Chat: buffer en DO storage, flush D1 en blocks de 50

## Extension futura

- `typing` por canal (solo suscritos) — ya cubierto
- Notificaciones push (email) — via Cloudflare Email Routing en Fase 6+
- Invitaciones real-time — tipo `invite` message en presence WS
