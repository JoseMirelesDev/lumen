# Checklist de implementacion

Seguimiento por fase. Marcar [x] al completar. Cada item tiene su
definicion de done en el archivo de la fase.

## Fase 1 — Seguridad base (`phases/01-security.md`)

### Backend
- [x] `src/rate-limit.ts` creado (Cache API sliding window)
- [x] Rate limits configurados por ruta (tabla 5.1 ARCHITECTURE.md)
- [x] CORS restringido (ALLOWED_ORIGINS, no wildcard)
- [x] Body size limit 1 MB
- [x] `GET /api/health` (D1 probe + version)
- [x] refresh_tokens table (migration 0003)
- [x] Login/register devuelven `{ token, refreshToken, user }`
- [x] `POST /api/auth/refresh` (rotation)
- [x] `POST /api/auth/logout` (revoke)
- [x] `DELETE /api/auth/sessions` (revoke all)
- [x] Access token TTL 1h

### Cliente
- [x] Guarda refreshToken (auth.rs)
- [x] On 401 -> refresh -> retry una vez (api.rs)
- [x] Logout llama POST /api/auth/logout
- [x] Refresco automatico de access token

### Tests
- [x] Rate limit: 11 requests -> 429
- [x] CORS negado para origin desconocido
- [x] Body > 1MB -> 413
- [x] Health check 200
- [x] Refresh rotation: token reusado -> 401
- [x] Logout revoca

## Fase 2 — CRUD completo (`phases/02-crud.md`)

### Migration 0003 aplicada
- [x] message_blocks
- [x] channels.topic + position
- [x] messages.edited_at + deleted_at + reply_to
- [x] servers.icon + invite_regenerated_at
- [x] users.avatar + password_version + deleted_at
- [x] refresh_tokens

### Rutas
- [x] `PATCH /api/me` (username)
- [x] `PUT /api/me/password`
- [x] `DELETE /api/me` (soft + revoke)
- [x] `PATCH /api/servers/:id` (name/icon)
- [x] `DELETE /api/servers/:id` (cascade, ?confirm=true)
- [x] `POST /api/servers/:id/leave`
- [x] `POST /api/servers/:id/invite` (regenera)
- [x] `DELETE /api/servers/:id/members/:userId` (kick)
- [x] `PATCH /api/channels/:id` (name/topic/position)
- [x] `DELETE /api/channels/:id`
- [x] `PATCH /api/messages/:id` (author)
- [x] `DELETE /api/messages/:id` (author o owner)
- [x] `DELETE /api/friends/:userId`
- [x] `DELETE /api/dms/:id` (soft per-user)

### Permisos
- [x] Owner-only: server/channel mutations
- [x] Author-only: message edit; author-or-owner: message delete
- [x] Owner no puede leave

### Cliente
- [x] Server context menu (edit/invite/leave/delete)
- [x] Channel context menu (edit/delete)
- [x] Message context menu (edit/delete) + placeholder "eliminado"
- [x] Friend remove
- [x] Settings (username/password)
- [x] Indicador "editado" en mensajes

### Tests
- [x] Smoke: CRUD servers/channels/messages/friends
- [x] Permisos 403
- [x] Cascade delete
- [x] Username conflict 409

## Fase 3 — Real-time y presencia (`phases/03-realtime.md`)

### Backend
- [x] wrangler.toml: LUMEN_PRESENCE_DO binding + migration v2
- [x] `src/do/PresenceHubDO.ts` creado (spec completa)
- [x] `GET /api/presence` upgrade route + getUserPresenceContext
- [x] Presence tags (userId + s:serverId) + attachments
- [x] ready snapshot (onlineFriends + servers + voiceChannels)
- [x] friend-online/offline/status broadcasts
- [x] voice-join/leave + voice-update broadcast
- [x] chat: buffer storage + ACK + broadcast + flush (50 o 5min)
- [x] alarm() flush todos los buffers
- [x] subscribe/unsubscribe (tag c:channelId)
- [x] dm-signal relay (P2P DM signaling)
- [x] Rate limits WS (chat 10/10s, typing 3/5s, voice 5/60s)
- [x] GET /api/channels/:id/messages con paginacion (before cursor) + merge buffer
- [x] last_seen update on disconnect (1 write)

### Protocol
- [x] packages/protocol: PresenceClientMessage/ServerMessage, MessageBlock, BufferedMessage

### Cliente
- [x] lumen-core: PresenceState + CoreEvent nuevos
- [x] lumen-core: conexion presence WS (connect al login, reconnect con backoff)
- [x] lumen-voice: create_data_channel("chat") en voice
- [x] lumen-voice: VoiceEvent::DataChannelMessage
- [x] lumen-voice: join_dm (data-only peer connection)
- [x] Slint: lista de online members (dot verde)
- [x] Slint: badge peers en voice channels
- [x] Slint: friends tab real-time
- [x] Slint: typing indicator
- [x] Slint: chat envio via WS + ACK handling + retransmision (3s timeout)
- [x] Slint: paginacion scroll up (cursor)

### Tests
- [x] Smoke: presence connect/ready/friend-online/voice-update
- [x] Smoke: chat RT + ACK + flush + paginacion
- [x] Rate limit chat
- [x] P2P DM typing sin servidor

## Fase 4 — OAuth y assets (`phases/04-oauth-assets.md`)

- [x] migration 0004 aplicada
- [x] Secrets OAuth puestos (4 client secrets + callback URL)
- [x] GET /api/oauth/:provider (state + redirect)
- [x] GET /api/oauth/:provider/callback (exchange + find/create user)
- [x] redirect desktop (lumen://) vs web
- [x] R2 binding en wrangler.toml
- [x] PUT /api/me/avatar (5MB, R2 + columna avatar)
- [x] PUT /api/servers/:id/icon (owner, R2)
- [x] GET /api/assets/:path+ (public, cache)
- [x] Deep link lumen:// en main.rs (parse argv)
- [x] Deep link auth/callback -> guarda token
- [x] UI: botones OAuth, avatares, iconos, settings avatar

### Tests
- [x] Flujo OAuth Google completo
- [x] Flujo OAuth GitHub completo
- [x] State reusado -> 400
- [x] Avatar > 5MB -> 413
- [x] Deep link reabre app autenticada

## Fase 5 — Moderacion (`phases/05-moderation.md`)

- [x] migration 0005 aplicada
- [x] POST/DELETE/GET /api/servers/:id/bans (owner)
- [x] Ban bloquea join e upgrade WS
- [x] POST/DELETE /api/blocks
- [x] POST /api/reports (rate 5/dia)
- [x] POST /api/servers/:id/transfer
- [x] Transferencia en server huerfano (owner soft-deleted)
- [x] Kick: broadcast + bloqueo en proximo acceso
- [x] UI: member list, ban/unban, block/report menus, transfer

### Tests
- [x] Ban -> join 403, upgrade 403
- [x] Unban -> join OK
- [x] Block -> DM/friend bloqueados
- [x] Report -> fila
- [x] Transfer -> permisos cambian

## Fase 6 — Polish (`phases/06-polish.md`)

Implementar en orden sugerido; cada item con su propia mini-acceptance:

- [x] 6.1 Replies (reply_to)
- [x] 6.2 Attachments (R2 + columna)
- [x] 6.3 Reactions
- [ ] 6.4 Pins  — NO implementado (misión: solo 6.1-6.3)
- [ ] 6.5 Search FTS5  — NO implementado (misión: solo 6.1-6.3 = reactions, replies, attachments; 6.4 Pins fuera del alcance indicado)
- [ ] 6.6 Roles y permisos (bitmask)  — NO implementado (misión: solo 6.1-6.3 = reactions, replies, attachments; 6.4 Pins fuera del alcance indicado)
- [ ] 6.7 Channel categories  — NO implementado (misión: solo 6.1-6.3 = reactions, replies, attachments; 6.4 Pins fuera del alcance indicado)
- [ ] 6.8 User search  — NO implementado (misión: solo 6.1-6.3 = reactions, replies, attachments; 6.4 Pins fuera del alcance indicado)
- [ ] 6.9 Admin panel (reports resolve)  — NO implementado (misión: solo 6.1-6.3 = reactions, replies, attachments; 6.4 Pins fuera del alcance indicado)
- [ ] 6.10 Migracion Go (trigger por metrica)  — NO implementado (misión: solo 6.1-6.3 = reactions, replies, attachments; 6.4 Pins fuera del alcance indicado)

## Cross-cutting (todas las fases)

- [x] BUDGET.md actualizado con mediciones reales por fase
- [x] docs/protocol.md actualizado (presence-v2 merge)
- [x] docs/dev-diary entrada por fase completada
- [x] .dev.vars.example actualizado (nuevos secrets)
- [x] worker-configuration.d.ts regenerado (wrangler types)
- [x] README de backend actualizado
- [x] Rate limit en CADA ruta nueva (regla 5)
- [x] Zero D1 writes para estado efimero (regla 1)
- [x] Hibernation API: sin estado en campos de instancia (regla 6)
- [x] Tags para routing, no iterar sockets (regla 7)

## Architecture governance (review 2026-08-13)

- [x] ADR-003..0010 leídos y seguidos (docs/decisions/)
- [x] Módulos puros en do/lib/* con unit tests (P3/P7)
- [x] Mutaciones de mensajes SOLO vía WS del DO (P2/ADR-0010); REST read-only
- [x] Cursor compuesto (last_at, id) en paginación (P5)
- [x] Migraciones forward-only, gates por fase (P6)
- [x] FTS5 no implementado sobre blocks (P4) — LIKE MVP o diferir a Go (cumplido: no se implementó)
- [x] Bounded contexts (ARCHITECTURE-REVIEW.md §2) respetados: un dueño por dominio
