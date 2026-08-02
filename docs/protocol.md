# Lumen signaling protocol — v1

Status: **contract** (cross-slice interface between `@lumen/backend` and `@lumen/desktop`; both import the types from `@lumen/protocol`).

## 1. Transport

- All real-time signaling runs over a single WebSocket per (client, channel) pair.
- URL: `wss://<worker>/api/ws/:channelId`, opened with `Authorization: Bearer <token>`.
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
| presence scoped to channel | global (cross-server) presence would need a per-user DO; v1 derives "online" from `last_seen` (updated on login) + live channel membership. Documented in ADR 0003. |

## 4. REST API (same origin as WS)

All JSON; auth via `Authorization: Bearer <token>` (HMAC-signed JWT, see ADR 0002).

| method | path | body | returns |
|---|---|---|---|
| POST | `/api/auth/register` | `{username, password}` | `201 {token, user}` |
| POST | `/api/auth/login` | `{username, password}` | `200 {token, user}` |
| GET | `/api/me` | — | `{user}` |
| POST | `/api/servers` | `{name}` | `201 {server, channels}` (creates `general` text + `General` voice) |
| GET | `/api/servers` | — | `[ServerWithChannels]` (member of) |
| POST | `/api/servers/:id/join` | `{inviteCode}` | `200 {server, channels}` |
| GET | `/api/servers/:id` | — | `{server, channels}` |
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
