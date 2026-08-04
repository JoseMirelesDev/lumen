# Deploying Lumen

Two deliverables: the Cloudflare backend (signaling + REST) and the desktop
bundle per platform. This document covers both, plus the free-tier budget
checks that gate the Fase 1 / Fase 5 Definitions of Done.

## 1. Backend (Cloudflare Workers + Durable Objects + D1)

### 1.1 One-time setup

```bash
cd apps/backend

# 1. Authenticate (interactive — needs a browser):
wrangler login

# 2. Create the D1 database and paste the returned id into wrangler.toml
#    ([[d1_databases]] database_id):
wrangler d1 create lumen-d1

# 3. Set secrets:
wrangler secret put AUTH_SECRET          # 32+ random bytes, e.g. `openssl rand -hex 32`
wrangler secret put REALTIME_TURN_KEY_ID # optional — Cloudflare Realtime TURN
wrangler secret put REALTIME_API_TOKEN   # optional — Cloudflare Realtime TURN
```

`wrangler.toml` ships with `database_id = 00000000-…` as a placeholder; local
dev via `wrangler dev` uses miniflare's own D1, so the placeholder is fine
until deploy. `migrations_dir = "migrations"` means `wrangler d1 migrations
apply lumen-d1` applies `0001_init.sql` and `0002_dm.sql` in order.

### 1.2 Deploy

```bash
cd apps/backend
wrangler d1 migrations apply lumen-d1   # schema: users, servers, channels (+dm kind), dm_members, friendships, messages
wrangler deploy                          # worker + LumenChannelDO (WS Hibernation)
```

Secrets (AUTH_SECRET, TURN) must be set before `wrangler deploy` — the worker
refuses to start signing JWTs without them.

### 1.3 Smoke against the deployed worker

```bash
# Point the smoke test at the live URL and run it:
BACKEND_URL=https://lumen-backend.<your-subdomain>.workers.dev pnpm smoke
```

The smoke suite covers auth, servers/channels/messages, friends, DMs, and a
two-WebSocket offer/answer exchange through the DO — 49 steps.

### 1. Free-tier budget checks (Fase 1 / Fase 5 DoDs)

After a few hours of idle + light use, the Cloudflare dashboard should show:

- **DO duration ≈ 0 GB-s** outside active calls. The DO calls `ctx.acceptWebSocket()`
  and hibernate-based handlers only (`webSocketMessage`, `webSocketClose`) — no
  timers, no storage polling — so an empty channel costs ~nothing. Verified via
  GraphQL: `durableObjectsInvocationsAdaptiveGroups` shows entries only in
  minutes with actual traffic; a live-but-idle socket (hibernated) adds zero
  `wallTime`.
- **Worker requests** well under 100k/day; **DO requests** under 100k/day
  (each WebSocket message counts 1/20 of a request on Hibernation APIs).
- **D1 reads** under 5M/day, writes under 100k/day.

## 1.5 Known production-only constraints

- **PBKDF2 iteration cap**: the Workers runtime (workerd) rejects
  `crypto.subtle.deriveBits` with more than **100,000 iterations** — 210k
  throws in production while passing under local miniflare (easy to miss:
  everything works in `wrangler dev`). `apps/backend/src/auth.ts` uses 100k.
  Symptom of exceeding it: register → 500, login with correct password → 401.
- **workers.dev edge cache**: GET responses on the workers.dev domain can be
  served stale for minutes (cache key ignores query strings). After a deploy,
  wait ~1 min or use a POST to verify new code — don't conclude a deploy
  failed from a single GET.

## 2. Desktop installers

Build on each target OS (Tauri produces native installers only on the matching
OS; there is no cross-compile path for these formats):

| OS      | Command (in `apps/desktop`)            | Artifacts                                        |
|---------|----------------------------------------|--------------------------------------------------|
| Linux   | `pnpm tauri build`                     | `.deb`, `.rpm`, `.AppImage`                      |
| Windows | `pnpm tauri build`                     | `.msi` (NSIS also available via `--bundles nsis`) |
| macOS   | `pnpm tauri build`                     | `.dmg`                                           |

Artifacts land in `apps/desktop/src-tauri/target/release/bundle/`.

Notes:

- `bundle.targets = "all"` is already set in `tauri.conf.json`; add
  `bundle > windows > wix`/`nsis` overrides only if you need custom install
  paths.
- **Signing**: macOS notarization needs a Developer ID + `APPLE_SIGNING_IDENTITY`
  env during build; Windows signing needs a code-signing cert via `signtool`.
  Both are CI-environment concerns — see the GitHub Actions workflow
  (`.github/workflows/ci.yml`) for the matrix (ubuntu/windows/macos) that runs
  typecheck + lint + build. Wire signing env vars there before distributing
  outside your own machines; unsigned builds work locally.
- Linux `.AppImage` requires `patchelf` (already a listed prerequisite).

### 2.1 Verifying an installer

```bash
# .deb
sudo apt install ./lumen_0.1.0_amd64.deb && lumen

# .AppImage
chmod +x ./Lumen_0.1.0_amd64.AppImage && ./Lumen_0.1.0_amd64.AppImage
```

The desktop app defaults to `http://localhost:8787`; on a real deployment,
enter the deployed worker URL on the login screen (persisted in localStorage)
or bake it at build time with `VITE_BACKEND_URL`.

## 3. End-to-end acceptance (Fase 5 DoD)

With the backend deployed and two desktop installs:

1. Account A registers, creates a server, creates a text + a voice channel.
2. Account B registers and joins by invite code.
3. A and B friend each other, open a DM, exchange text, and join a DM voice call.
4. B shares screen into the voice channel; A sees the video track.
5. Dashboard checks (1.4) still pass after the session.
