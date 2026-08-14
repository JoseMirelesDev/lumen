/**
 * Rate limiting — Cache API sliding window (ADR-0009).
 *
 * Key: `rl:<ip>:<METHOD>:<path>`, value: request count so far in the window.
 * The Cache API eviction only resets a window (fail-open, acceptable per
 * ADR-0009); limits are conservative by default so an evicted window is
 * never a real exposure.
 *
 * Cost: 1 cache read + 1 cache write per request. `cf-connecting-ip` is the
 * Cloudflare-attested client IP (not spoofable behind their proxy).
 */

export interface RateLimitConfig {
  limit: number;
  windowSeconds: number;
}

/** Per-route limits (ARCHITECTURE.md §5.1). Unlisted routes get DEFAULT_LIMIT. */
export const RATE_LIMITS: Record<string, RateLimitConfig> = {
  "POST:/api/auth/register": { limit: 3, windowSeconds: 3600 },
  "POST:/api/auth/login": { limit: 10, windowSeconds: 300 },
  "POST:/api/auth/refresh": { limit: 30, windowSeconds: 300 },
  "GET:/api/oauth/:provider": { limit: 10, windowSeconds: 300 },
  "GET:/api/oauth/:provider/callback": { limit: 10, windowSeconds: 300 },
  "POST:/api/servers": { limit: 5, windowSeconds: 3600 },
  "POST:/api/servers/:id/channels": { limit: 10, windowSeconds: 3600 },
  "POST:/api/friends/requests": { limit: 10, windowSeconds: 3600 },
  "POST:/api/dms": { limit: 10, windowSeconds: 3600 },
  "POST:/api/channels/:id/messages": { limit: 30, windowSeconds: 30 },
  "PATCH:/api/messages/:id": { limit: 30, windowSeconds: 60 },
  "DELETE:/api/messages/:id": { limit: 30, windowSeconds: 60 },
  "POST:/api/reports": { limit: 5, windowSeconds: 86400 },
};

/** Wildcard: 100 req/min per IP. */
export const DEFAULT_LIMIT: RateLimitConfig = { limit: 100, windowSeconds: 60 };

/**
 * Match the request path against the configured per-route table by position:
 * `/api/servers/abc/channels` → `POST:/api/servers/:id/channels` so the
 * per-route limits apply to their param variants. Falls back to the raw
 * `METHOD:/path` key (DEFAULT_LIMIT applies).
 */
export function rateLimitKey(method: string, pathname: string): string {
  const segments = pathname.split("/").filter(Boolean);
  for (const key of Object.keys(RATE_LIMITS)) {
    if (!key.startsWith(`${method}:`)) continue;
    const template = key.slice(method.length + 1).split("/").filter(Boolean);
    if (template.length !== segments.length) continue;
    let ok = true;
    for (let i = 0; i < template.length; i++) {
      const t = template[i]!;
      if (t.startsWith(":")) continue;
      if (t !== segments[i]) {
        ok = false;
        break;
      }
    }
    if (ok) return key;
  }
  return `${method}:${pathname}`;
}

export interface RateLimitResult {
  ok: boolean;
  retryAfterSeconds: number;
}

/**
 * The Workers runtime exposes exactly one cache, `caches.default`; the
 * TypeScript WebWorker lib types `CacheStorage` without it. Resolved at call
 * time so tests can stub the `caches` global before exercising the module.
 */
function defaultCache(): Cache {
  return (caches as typeof caches & { default: Cache }).default;
}

/**
 * Returns whether the request is under the limit. On `ok: false` the caller
 * must answer 429 `rate_limited` with `Retry-After: retryAfterSeconds`.
 */
export async function enforceRateLimit(
  request: Request,
  env: Env,
): Promise<RateLimitResult> {
  // Dev/test-only escape hatch (set in local .dev.vars, NEVER in production):
  // the smoke suite legitimately registers ~10 users, which would trip the
  // 3/h register limit. The limiter logic itself is unit-tested.
  if (env.LUMEN_RATE_LIMIT_DISABLED === "true") {
    return { ok: true, retryAfterSeconds: 0 };
  }
  const ip = request.headers.get("cf-connecting-ip") ?? "unknown";
  const method = request.method;
  const path = new URL(request.url).pathname;
  const key = `rl:${ip}:${method}:${path}`;
  // Cache API keys MUST be full URLs (or Request objects) — a bare string
  // throws TypeError → uncaught 1101 outside the handler's try/catch. The
  // synthetic host is never resolved; the path is the real key.
  const cacheKey = `https://rate-limit/${encodeURIComponent(key)}`;
  const cfgKey = rateLimitKey(method, path);
  const cfg = RATE_LIMITS[cfgKey] ?? DEFAULT_LIMIT;

  const cached = await defaultCache().match(cacheKey);
  let count = 0;
  if (cached) {
    const text = await cached.text();
    const parsed = Number.parseInt(text, 10);
    if (Number.isFinite(parsed)) count = parsed;
  }
  if (count >= cfg.limit) {
    return { ok: false, retryAfterSeconds: cfg.windowSeconds };
  }
  count += 1;
  await defaultCache().put(
    cacheKey,
    new Response(String(count), {
      headers: {
        "cache-control": `max-age=${cfg.windowSeconds}`,
        "x-rl-limit": String(cfg.limit),
        "x-rl-remaining": String(Math.max(0, cfg.limit - count)),
      },
    }),
  );
  return { ok: true, retryAfterSeconds: cfg.windowSeconds };
}
