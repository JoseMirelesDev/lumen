# Lumen

Ultralight Discord-like desktop app for groups of up to 4 people per voice channel.
Voice runs P2P (WebRTC mesh) over Cloudflare's free tier; the
Cloudflare Worker + Durable Objects handle signaling only.

- **Desktop**: Slint 1 (native Rust UI, `apps/lumen-slint`, bin `lumen`).
  The previous Tauri 2 + Svelte 5 client (`apps/desktop`) is **DEPRECATED /
  discontinued** — kept only as reference until the Fase 6 cutover removes it.
- **Signaling**: Cloudflare Workers REST + Durable Objects (WebSocket Hibernation) + D1
- **Media**: native voice in Rust (cpal ↔ OPUS ↔ webrtc-rs 0.20), full-mesh P2P audio to up to 3 peers over the WebSocket relay; screen share deferred (video track seam left open in the client)
- **NAT**: `stun.cloudflare.com` primary; Cloudflare Realtime TURN fallback

## Repo layout

```
apps/backend        Cloudflare Worker: REST API + LumenChannelDO + D1 migrations
apps/lumen-slint    Slint desktop client (native Rust UI, bin `lumen`)
apps/desktop        Tauri 2 + Svelte 5 client — DEPRECATED, discontinued
packages/protocol   Shared signaling/REST types (single source of truth)
docs/               protocol spec, ADRs, measured performance numbers
```

## Prerequisites

- Node.js ≥ 22, pnpm ≥ 10
- Rust stable (for the desktop app)
- (Tauri, deprecated) Linux system deps: `libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev patchelf` — only if you still build `apps/desktop`

## Quickstart

```bash
pnpm install

# Backend (local, miniflare): REST + DO + D1 at http://localhost:8787
pnpm --filter @lumen/backend dev          # apply migrations first, see below

# Migrations (local D1)
pnpm --filter @lumen/backend migrate:local

# Desktop (dev, native Slint client)
cargo run -p lumen-desktop

# Desktop (dev, DEPRECATED Tauri client)
pnpm --filter @lumen/desktop tauri dev
```

Backend secrets for local dev: copy `apps/backend/.dev.vars.example` to
`apps/backend/.dev.vars` and set `AUTH_SECRET` (32+ random bytes). Deployment
instructions: see `docs/deploy.md` (written in Fase 6).

The desktop app talks to the backend URL shown on the login screen
(`http://localhost:8787` by default; editable there and persisted, or set
`VITE_BACKEND_URL` at build time).

**Linux WebKit note (Tauri, deprecated)**: on older Intel iGPUs (e.g. HD 4600) the WebKitGTK window
may never map without `WEBKIT_DISABLE_COMPOSITING_MODE=1` in the environment.
Set it when running `tauri dev` if the window does not appear. This is a dev
environment quirk, not a code change — reported as a known risk in Fase 4.
The Slint client has its own software renderer fallback (`SLINT_BACKEND`).

## Phases

Tracked as a persistent project todo (Fase 0–6), each gated on its Definition of
Done. See the project brief and `docs/` for protocol, decisions, and numbers.
