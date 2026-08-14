import { afterEach, describe, expect, it, vi } from "vitest";

import { enforceRateLimit, rateLimitKey, RATE_LIMITS } from "../src/rate-limit";

/**
 * In-memory Cache API stand-in. Workers' Cache is opaque and evicts freely;
 * the fake models the two behaviors the limiter relies on: `match` returns
 * the stored response (with its body = request count) and `put` stores it.
 */
function fakeCache(): {
  cache: { match: (k: string) => Promise<Response | undefined>; put: (k: string, r: Response) => Promise<void> };
  store: Map<string, string>;
} {
  const store = new Map<string, string>();
  return {
    store,
    cache: {
      async match(k: string) {
        const body = store.get(k);
        return body === undefined ? undefined : new Response(body);
      },
      async put(k: string, r: Response) {
        store.set(k, await r.text());
      },
    },
  };
}

function req(method: string, path: string, ip = "203.0.113.7"): Request {
  return new Request(`https://lumen.test${path}`, {
    method,
    headers: { "cf-connecting-ip": ip },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("rate limit (Cache API sliding window)", () => {
  it("allows requests up to the default limit, then blocks", async () => {
    const { cache } = fakeCache();
    vi.stubGlobal("caches", { default: cache });
    for (let i = 0; i < 100; i++) {
      const r = await enforceRateLimit(req("GET", "/api/whatever"), {} as Env);
      expect(r.ok).toBe(true);
    }
    const blocked = await enforceRateLimit(req("GET", "/api/whatever"), {} as Env);
    expect(blocked.ok).toBe(false);
    expect(blocked.retryAfterSeconds).toBe(60);
  });

  it("blocks the 11th login request (10 per 5 min)", async () => {
    const { cache } = fakeCache();
    vi.stubGlobal("caches", { default: cache });
    for (let i = 0; i < 10; i++) {
      expect((await enforceRateLimit(req("POST", "/api/auth/login"), {} as Env)).ok).toBe(true);
    }
    const r = await enforceRateLimit(req("POST", "/api/auth/login"), {} as Env);
    expect(r.ok).toBe(false);
    expect(r.retryAfterSeconds).toBe(300);
  });

  it("applies per-route limits to param paths (POST servers/:id/channels)", async () => {
    const { cache } = fakeCache();
    vi.stubGlobal("caches", { default: cache });
    for (let i = 0; i < 10; i++) {
      expect(
        (await enforceRateLimit(req("POST", "/api/servers/abc-123/channels"), {} as Env)).ok,
      ).toBe(true);
    }
    expect((await enforceRateLimit(req("POST", "/api/servers/abc-123/channels"), {} as Env)).ok).toBe(false);
    // A different route under the same IP is unaffected.
    expect((await enforceRateLimit(req("GET", "/api/servers/abc-123"), {} as Env)).ok).toBe(true);
  });

  it("keys by IP — different IPs have independent windows", async () => {
    const { cache } = fakeCache();
    vi.stubGlobal("caches", { default: cache });
    for (let i = 0; i < 10; i++) {
      await enforceRateLimit(req("POST", "/api/auth/login", "1.1.1.1"), {} as Env);
    }
    expect((await enforceRateLimit(req("POST", "/api/auth/login", "1.1.1.1"), {} as Env)).ok).toBe(false);
    expect((await enforceRateLimit(req("POST", "/api/auth/login", "2.2.2.2"), {} as Env)).ok).toBe(true);
  });
});

describe("rateLimitKey", () => {
  it("matches configured route templates by position", () => {
    expect(rateLimitKey("POST", "/api/servers/xyz/channels")).toBe("POST:/api/servers/:id/channels");
    expect(rateLimitKey("POST", "/api/servers")).toBe("POST:/api/servers");
    expect(rateLimitKey("GET", "/api/servers/xyz")).toBe("GET:/api/servers/xyz"); // no table entry → raw
  });

  it("has every documented route limit configured", () => {
    // ARCHITECTURE.md §5.1 — the documented set must exist in the table.
    for (const key of [
      "POST:/api/auth/register",
      "POST:/api/auth/login",
      "POST:/api/servers",
      "POST:/api/servers/:id/channels",
      "POST:/api/friends/requests",
      "POST:/api/dms",
      "POST:/api/channels/:id/messages",
      "PATCH:/api/messages/:id",
      "DELETE:/api/messages/:id",
    ]) {
      expect(RATE_LIMITS[key], `missing limit for ${key}`).toBeDefined();
    }
  });
});
