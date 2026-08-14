# Lumen backend — Cloudflare Worker

REST signaling API + WebSockets (voice signaling v1 + presence v2) + D1 + R2.

## Estructura

```
src/
  index.ts              — Worker entry + todas las rutas REST + WS upgrades
  auth.ts               — PBKDF2 (100k iter — máximo de workerd), JWT HS256 1h,
                          refresh tokens rotativos (SHA-256 en D1, 30 días)
  db.ts                 — queries D1 tipadas
  router.ts             — path router (método + :params, :path+ wildcard)
  validation.ts         — validadores + invite codes
  rate-limit.ts         — Cache API sliding window (per-route + wildcard 100/min)
  realtime.ts           — credenciales ephemerales de TURN (Cloudflare Realtime)
  do/
    ChannelDO.ts        — signaling de voz (v1, mesh máx 4 peers)
    PresenceHubDO.ts    — presencia global + buffer de chat + relay DM (v2)
    lib/                — lógica pura (buffer, ws-rate-limit, presence-utils)
migrations/             — D1 forward-only (SPEC en plans/backend-v2/migrations/SPEC.md)
test/
  smoke-ws.mjs          — E2E: auth, CRUD, voice WS, presencia, chat RT, flush,
                          moderación, OAuth shape, assets, Fase 6
  *.test.ts             — unit tests (vitest)
```

## Desarrollo

```bash
pnpm install
cp .dev.vars.example .dev.vars        # AUTH_SECRET obligatorio
pnpm dev                              # wrangler dev --local --port 8787
pnpm migrate:local                    # aplicar migraciones a la D1 local
pnpm smoke                            # requiere el dev server corriendo
pnpm test                             # vitest
pnpm lint && pnpm typecheck
npx wrangler types                    # regenerar worker-configuration.d.ts
```

## Despliegue

```bash
pnpm migrate:remote                   # wrangler d1 migrations apply lumen-d1 --remote
pnpm deploy
```

## Secrets (wrangler secret put)

| Secret | Obligatorio | Uso |
|---|---|---|
| `AUTH_SECRET` | sí | HMAC de los JWT (32+ bytes hex) |
| `REALTIME_TURN_KEY_ID` / `REALTIME_API_TOKEN` | no | TURN de Cloudflare Realtime (fallback STUN) |
| `GOOGLE_CLIENT_ID` / `GOOGLE_CLIENT_SECRET` | no (Fase 4) | OAuth Google |
| `GITHUB_CLIENT_ID` / `GITHUB_CLIENT_SECRET` | no (Fase 4) | OAuth GitHub |
| `OAUTH_CALLBACK_URL` | no (Fase 4) | base del callback, p.ej. `https://api.dominio.com/api/oauth` |
| `WEB_CLIENT_URL` | no (Fase 4) | origen del web client post-OAuth |

`LUMEN_RATE_LIMIT_DISABLED=true` solo en `.dev.vars` local (desactiva los rate
limits para que el smoke suite pueda registrar ~10 usuarios). **Nunca en
producción.**

## Arquitectura

- 2 Durable Objects: `LumenChannelDO` (voz, por canal) y `PresenceHubDO`
  (singleton "hub": presencia, buffer de chat, relay DM). Hibernation API
  estricta: estado en attachments/tags/storage, nunca en campos de instancia.
- Chat: buffer 50 msgs → `message_blocks` (1 write por block). Mutaciones
  solo por WS (ADR-0010). REST read-only para mensajes.
- Presencia: tags `userId` / `s:<serverId>`; suscripciones a canales en el
  attachment (los tags son inmutables post-accept).
- Rate limits en CADA ruta (REST: Cache API; WS: contadores en attachment).
- Decisiones: ADRs 0003-0010 en `docs/decisions/`; presupuesto y límites en
  `plans/backend-v2/BUDGET.md`.
