# Lumen

Ultralight Discord-like desktop app for groups of up to 4 people per voice channel.
Voice + screen share run P2P (WebRTC mesh) over Cloudflare's free tier; the
Cloudflare Worker + Durable Objects handle signaling only.

- **Desktop**: Tauri 2 (Rust) + Svelte 5 (TypeScript) on the OS-native WebView
- **Signaling**: Cloudflare Workers REST + Durable Objects (WebSocket Hibernation) + D1
- **Media**: browser WebRTC inside the WebView — mesh up to 3 peers, RNNoise noise suppression
- **NAT**: `stun.cloudflare.com` primary; Cloudflare Realtime TURN fallback

## Repo layout

```
apps/backend        Cloudflare Worker: REST API + LumenChannelDO + D1 migrations
apps/desktop        Tauri 2 + Svelte 5 client
packages/protocol   Shared signaling/REST types (single source of truth)
docs/               protocol spec, ADRs, measured performance numbers
```

## Prerequisites

- Node.js ≥ 22, pnpm ≥ 10
- Rust stable (for the desktop app)
- Tauri Linux system deps: `libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev librsvg2-dev patchelf`

## Quickstart

```bash
pnpm install

# Backend (local, miniflare): REST + DO + D1 at http://localhost:8787
pnpm --filter @lumen/backend dev          # apply migrations first, see below

# Migrations (local D1)
pnpm --filter @lumen/backend migrate:local

# Desktop (dev, hot-reload frontend)
pnpm --filter @lumen/desktop tauri dev
```

Backend secrets for local dev: copy `apps/backend/.dev.vars.example` to
`apps/backend/.dev.vars` and set `AUTH_SECRET` (32+ random bytes). Deployment
instructions: see `docs/deploy.md` (written in Fase 6).

## Phases

Tracked as a persistent project todo (Fase 0–6), each gated on its Definition of
Done. See the project brief and `docs/` for protocol, decisions, and numbers.
