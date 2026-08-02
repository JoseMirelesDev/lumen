# ADR 0001 — Stack decisions (Lumen)

Status: accepted · Date: 2026-08-02

## Context

The brief leaves several choices open (name, frontend framework, RNNoise library, folder layout). Decisions below are committed and recorded so they are not reopened without cause.

## Decisions

### Name: Lumen
"Light" in Latin; matches the ultralight mission. Used for: workspace packages (`@lumen/*`), D1 database `lumen-d1` (binding `LUMEN_D1`), Durable Object instances prefixed `lumen-` (`lumen-<channelId>`), repo can be renamed `lumen` on push.

### Frontend: Svelte 5 (TypeScript) over React
- Svelte 5 compiled runtime is ~10 KB vs ~45 KB (react-dom) gzipped — matters on modest hardware.
- The UI surface is small (server/channel lists, chat, call bar); no rich ecosystem needed.
- Media/WebRTC code is framework-agnostic; reactivity needs are simple (stores).
- Trade-off accepted: smaller ecosystem than React; Svelte 5 (runes) is stable in 2026.
- Rejected: React — heavier runtime, no compensating need. Solid — fine but adds a compiler/kernel dep for no benefit here.

### Noise suppression: `@sapphi-red/web-noise-suppressor`
- Drop-in `AudioWorkletNode` wrapper around RNNoise WASM (`AudioWorkletNode` in, `AudioWorkletNode` out) — far better DX than `@jitsi/rnnoise-wasm`, which ships a raw WASM module requiring manual AudioWorklet assembly and lifecycle handling.
- Both implement the same RNNoise algorithm (48 kHz mono frame-based denoiser); the difference is integration cost.
- Re-evaluated in Fase 3 if Vite bundling of the worklet/WASM assets proves problematic.

### Monorepo: pnpm workspaces (`apps/*`, `packages/*`)
- Available locally (pnpm 10.33); strict dependency graph; `@lumen/protocol` shared as source (no build step — both consumers bundle TS via Vite/esbuild).

### Backend language: TypeScript on wrangler v4
- Mandated by stack. `nodejs_compat` flag on.

### Auth: PBKDF2-SHA256 password hashing + HMAC-SHA256 signed JWT (Web Crypto only)
- Zero runtime deps; no native modules on Workers.
- Passwords: PBKDF2 210k iterations, 16-byte random salt, stored as `salt:hash` hex.
- Tokens: 3-part `base64url` JWT with `sub` (userId), `iat`, `exp` (7 days), signed with secret binding `AUTH_SECRET`.

### TURN/STUN: Cloudflare Realtime ephemeral credentials proxied by the Worker
- Client never sees long-lived TURN credentials. `GET /api/realtime/config` calls the Realtime ephemeral-key API (`POST https://rtc.live.cloudflare.com/v1/turn/keys/<keyId>/credentials/generate`, ttl 86400) and returns `{iceServers}`.
- If `REALTIME_TURN_KEY_ID`/`REALTIME_API_TOKEN` secrets are absent, falls back to `{iceServers:[{urls:"stun:stun.cloudflare.com"}]}` so local dev works with zero account setup.

### Presence scoping (v1)
Global cross-server presence would need one DO per user (WS + state per online user). v1 ships: live presence inside shared channels (WS `presence` messages + peer list) and `last_seen` on users (one D1 write per login). Global presence = v1.1 candidate, documented in protocol.md.

### Folder layout
```
apps/backend     Cloudflare Worker + DO + D1 migrations
apps/desktop     Tauri 2 + Svelte 5 client
packages/protocol shared protocol/REST types (single source of truth)
docs/            protocol spec + ADRs + performance numbers
```
