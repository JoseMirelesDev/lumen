# Backend V2 — Plan de implementacion

> Documento de integracion del cliente: **[CLIENT-SLINT.md](CLIENT-SLINT.md)**
> — mapea cada fase a los cambios concretos en lumen-core, lumen-voice,
> apps/lumen-slint (EventBus, UiController, model.rs, .slint).
>
> **Decisiones**: ADR-003..0010 en `docs/decisions/` (singleton DO, message
> blocks, chat WS+ACK, P2P, refresh tokens, Go trigger, rate limiting,
> message mutations). **Health audit**: [ARCHITECTURE-REVIEW.md](ARCHITECTURE-REVIEW.md).

Estado: **PLANIFICADO** | Inicio: 2026-08-13

## Objetivo

Llevar el backend de Lumen desde su estado actual (auth basica, CRUD parcial,
voice signaling) a un nivel de produccion competitivo. Todo dentro del free
tier de Cloudflare.

## Arquitectura target

```
Cliente (Slint/Rust)
  |
  |-- REST (HTTPS) -----------> Worker (chi-style router)
  |                                |-- D1 (persistence)
  |                                |-- R2 (avatars, files)
  |                                |-- Cache API (read cache)
  |
  |-- WS /api/presence -------> PresenceHubDO (singleton)
  |     1 socket per session       |-- Presencia (tags + attachments)
  |     toda la sesion             |-- Chat buffer + flush (packed blocks)
  |                                |-- Voice occupancy
  |                                |-- Friend status
  |                                |-- DM signaling relay
  |                                |-- Rate limiting (per socket)
  |
  |-- WS /api/ws/:channelId --> ChannelDO (per voice channel)
  |     solo durante voice call    |-- WebRTC signaling relay
  |
  |-- WebRTC DataChannel -----> Otro peer (P2P directo)
        DM typing                  Sin costo servidor
        DM real-time delivery
        In-call chat
```

## Fases

| # | Nombre | Prioridad | Dependencia | Archivos principales |
|---|--------|-----------|-------------|---------------------|
| 1 | Seguridad base | Critica | Ninguna | index.ts, auth.ts, router.ts |
| 2 | CRUD completo | Critica | Ninguna | index.ts, db.ts, migration 0003 |
| 3 | Real-time y presencia | Alta | Fase 2 | PresenceHubDO.ts, protocol, client |
| 4 | OAuth y assets | Media | Fase 1 | auth.ts, R2, migration 0004 |
| 5 | Moderacion | Media | Fase 2 | db.ts, migration 0005 |
| 6 | Polish | Baja | Fase 3 | Incremental |

Fases 1 y 2 son paralelas (no se tocan). Fase 3 depende de 2 (necesita
las rutas de CRUD para que los datos de presencia sean coherentes). Fase 4
depende de 1 (security hardening antes de OAuth). Fases 5-6 son incrementales.

## Restricciones del free tier

Ver [BUDGET.md](BUDGET.md) para el desglose completo.

| Recurso | Limite | Uso estimado (500 users) | Techo |
|---------|--------|--------------------------|-------|
| Worker requests | 100k/dia | ~8,000 | ~6,000 users |
| DO requests | 100k/dia | ~4,300 | ~20,000 users |
| D1 writes | 100k/dia | ~1,500 (con buffer 50x) | ~50,000 users |
| D1 reads | 5M/dia | ~5,300 | Irrelevante |
| D1 storage | 5 GB | <100 MB | Irrelevante |
| R2 storage | 10 GB | <1 GB | Irrelevante |
| R2 Class A ops | 1M/mes | <50k | Irrelevante |
| R2 Class B ops | 10M/mes | <200k | Irrelevante |
| DO storage | 1 GB | <10 MB (buffers) | Irrelevante |

## Reglas de implementacion

1. **Zero D1 writes para operaciones efimeras.** Typing, presencia, voice
   state: solo DO memory/attachments. Nunca D1.

2. **Message buffer obligatorio.** Cada mensaje de chat pasa por el buffer
   del PresenceHubDO. Flush a D1 como packed block (50 msgs/row). Nunca
   INSERT directo a messages.

3. **P2P first para DMs.** Si ambos peers estan online y tienen DataChannel,
   typing y delivery van P2P. Server solo persiste.

4. **Cache API para reads repetidos.** Server detail, channel list, friend
   list: cache 60s. Invalida en mutacion.

5. **Rate limit en cada ruta.** REST: Cache API sliding window. WS: counter
   en socket attachment. Sin excepciones.

6. **Hibernation API estricta.** Ningun DO mantiene estado en campos de
   instancia. Todo en state.storage o socket attachments. Constructor se
   re-ejecuta en cada wake.

7. **Tags para routing.** PresenceHubDO usa tags (userId, "s:serverId")
   para broadcast. Nunca iterar todos los sockets.

8. **Broadcast es gratis.** Mensajes DO->client no cuestan. Disenar para
   maximizar broadcasts y minimizar mensajes client->DO.

## Mapa de archivos

```
apps/backend/
  src/
    index.ts              -- Worker entry + REST routes
    auth.ts               -- Password hashing + JWT + OAuth exchange
    db.ts                 -- D1 typed queries
    router.ts             -- Path router + ApiError
    validation.ts         -- Input validators
    rate-limit.ts         -- NEW: Cache API rate limiter
    env.d.ts              -- Env bindings
    do/
      ChannelDO.ts        -- Voice signaling (existing, minor changes)
      PresenceHubDO.ts    -- NEW: presence + chat buffer + friend status
  migrations/
    0001_init.sql         -- Existing
    0002_dm.sql           -- Existing
    0003_crud.sql         -- NEW: topics, soft deletes, blocks
    0004_oauth.sql        -- NEW: OAuth columns, oauth_states
    0005_moderation.sql   -- NEW: bans, reports
  wrangler.toml           -- Add R2 + PresenceHubDO bindings
  test/
    smoke-ws.mjs          -- Update for new routes

packages/protocol/
  src/index.ts            -- Add presence types, message block types

crates/lumen-core/
  src/
    protocol.rs           -- Mirror protocol changes
    state.rs              -- Add presence state fields
    api.rs                -- Add new API calls

crates/lumen-voice/
  src/
    signaling.rs          -- Add data channel support
    client.rs             -- Add DataChannel to peer connections

apps/lumen-slint/
  src/
    controller.rs         -- Handle new events
    model.rs              -- New UI state fields
    voice.rs              -- Data channels
  ui/                     -- Slint UI changes
```
