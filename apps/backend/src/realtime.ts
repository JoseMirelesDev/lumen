import type { RealtimeConfig } from "@lumen/protocol";

/**
 * Ephemeral ICE server config.
 *
 * When REALTIME_TURN_KEY_ID + REALTIME_API_TOKEN are set, fetches Cloudflare
 * Realtime TURN credentials and caches them ~15 min in a module-level variable
 * (fine for a single isolate). Otherwise serves a STUN-only fallback.
 */

const CACHE_TTL_MS = 15 * 60 * 1000;
const FALLBACK: RealtimeConfig = { iceServers: [{ urls: "stun:stun.cloudflare.com" }] };

let cache: { config: RealtimeConfig; expiresAt: number } | null = null;

export async function getRealtimeConfig(env: Env): Promise<RealtimeConfig> {
  const keyId = env.REALTIME_TURN_KEY_ID;
  const apiToken = env.REALTIME_API_TOKEN;
  if (!keyId || !apiToken) return FALLBACK;

  if (cache && cache.expiresAt > Date.now()) return cache.config;

  try {
    const res = await fetch(
      `https://rtc.live.cloudflare.com/v1/turn/keys/${encodeURIComponent(keyId)}/credentials/generate`,
      {
        method: "POST",
        headers: {
          Authorization: `Bearer ${apiToken}`,
          "Content-Type": "application/json",
        },
        body: JSON.stringify({ ttl: 86400 }),
      },
    );
    if (res.ok) {
      const config = (await res.json()) as RealtimeConfig;
      if (Array.isArray(config.iceServers) && config.iceServers.length > 0) {
        cache = { config, expiresAt: Date.now() + CACHE_TTL_MS };
        return config;
      }
    }
  } catch {
    // Network/parse failure → fall through to STUN fallback.
  }
  return FALLBACK;
}
