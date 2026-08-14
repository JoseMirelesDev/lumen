# Fase 1 — Seguridad base

Prioridad: **CRITICA** | Dependencias: ninguna | Duracion estimada: 2-3 dias

## Objetivo

Hardening del Worker existente. Sin esto, ninguna otra fase sale a
produccion. No cambia funcionalidad visible, solo protege lo que hay.

## Archivos

| Archivo | Cambio |
|---|---|
| `src/rate-limit.ts` | **NUEVO** — rate limiter con Cache API |
| `src/index.ts` | Integrar rate limits + CORS + body limit + health |
| `src/auth.ts` | Refresh tokens + logout + revocacion |
| `src/router.ts` | Middleware chain (rate limit, CORS, body limit) |
| `src/db.ts` | refresh_tokens queries |
| `migrations/0003_crud.sql` | Solo la parte refresh_tokens |
| `src/env.d.ts` | Tipos nuevos |

## Tareas

### 1.1 Rate limiter (Cache API)

```typescript
// src/rate-limit.ts
export interface RateLimitConfig {
  limit: number;
  windowSeconds: number;
}

const RATE_LIMITS: Record<string, RateLimitConfig> = {
  "POST:/api/auth/register":   { limit: 3, windowSeconds: 3600 },
  "POST:/api/auth/login":      { limit: 10, windowSeconds: 300 },
  "POST:/api/auth/refresh":    { limit: 30, windowSeconds: 300 },
  "POST:/api/servers":         { limit: 5, windowSeconds: 3600 },
  "POST:/api/servers/:id/channels": { limit: 10, windowSeconds: 3600 },
  "POST:/api/friends/requests":{ limit: 10, windowSeconds: 3600 },
  "POST:/api/dms":             { limit: 10, windowSeconds: 3600 },
  "POST:/api/channels/:id/messages": { limit: 30, windowSeconds: 30 },
  "PATCH:/api/messages/:id":   { limit: 30, windowSeconds: 60 },
  "DELETE:/api/messages/:id":  { limit: 30, windowSeconds: 60 },
};

// wildcard: 100 req/min por IP
const DEFAULT_LIMIT: RateLimitConfig = { limit: 100, windowSeconds: 60 };

export async function enforceRateLimit(
  request: Request, env: Env
): Promise<boolean> {
  const ip = request.headers.get("cf-connecting-ip") ?? "unknown";
  const method = request.method;
  const path = new URL(request.url).pathname;
  const key = `rl:${ip}:${method}:${path}`;

  const cached = await caches.default.match(key);
  let count = 1;
  if (cached) {
    const ttl = Number(cached.headers.get("x-rl-ttl") ?? 60);
    count = Number(await cached.text()) + 1;
    if (count > ttl) return false;  // limit reached
  }
  const resp = new Response(String(count), {
    headers: {
      "cache-control": "max-age=60",
      "x-rl-ttl": String(DEFAULT_LIMIT.limit),
    },
  });
  await caches.default.put(key, resp);
  return true;
}
```

Verificacion: `test/rate-limit.test.ts` — 11 requests en 1s -> 429 en el 11.

### 1.2 CORS restringido

```typescript
// en index.ts
const ALLOWED_ORIGINS = new Set([
  "http://localhost:8787",
  "http://localhost:5173",
  // dominio de produccion cuando exista
]);

function corsHeaders(request: Request): Record<string, string> {
  const origin = request.headers.get("origin");
  if (!origin) return {}; // desktop native, curl: sin CORS (no aplica)
  if (ALLOWED_ORIGINS.has(origin)) {
    return {
      "access-control-allow-origin": origin,
      "access-control-allow-methods": "GET, POST, PUT, PATCH, DELETE, OPTIONS",
      "access-control-allow-headers": "authorization, content-type",
      "access-control-max-age": "86400",
      "vary": "origin",
    };
  }
  return {}; // sin headers -> browser bloquea
}
```

Nota: el cliente Slint es nativo (no browser). Envía `Origin` solo si se
ejecuta en un WebView. Para requests sin Origin, no aplicar CORS (el
browser no puede hacerlos, asi que no hay riesgo CSRF por CORS; la
proteccion CSRF real es el JWT en Authorization header).

### 1.3 Body size limit

```typescript
// en readJson() y en el fetch handler:
const MAX_BODY = 1024 * 1024; // 1 MB
if (request.headers.get("content-length") &&
    Number(request.headers.get("content-length")) > MAX_BODY) {
  throw new ApiError(413, "payload_too_large");
}
// para chunked: leer y verificar longitud
```

### 1.4 Health check

```typescript
router.get("/api/health", false, async (ctx) => {
  const d1 = await ctx.env.LUMEN_D1.prepare("SELECT 1").first().catch(() => null);
  return json({
    status: d1 ? "ok" : "degraded",
    d1: d1 ? "ok" : "error",
    version: "2.0.0",
    timestamp: new Date().toISOString(),
  });
});
```

### 1.5 Refresh tokens

```sql
-- migrations/0003_crud.sql (parte 1)
CREATE TABLE refresh_tokens (
  token_hash TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
  expires_at TEXT NOT NULL,
  revoked_at TEXT
);
CREATE INDEX idx_refresh_user ON refresh_tokens(user_id);
```

```typescript
// auth.ts additions
const ACCESS_TTL = 3600;         // 1h
const REFRESH_TTL = 30 * 86400;  // 30d

export async function createRefreshToken(
  db: D1Database, userId: string
): Promise<string> {
  const token = crypto.randomUUID() + crypto.randomUUID();
  const hash = await sha256(token);
  await db.prepare(
    `INSERT INTO refresh_tokens (token_hash, user_id, expires_at)
     VALUES (?, ?, ?)`
  ).bind(hash, userId, new Date(Date.now() + REFRESH_TTL * 1000).toISOString())
   .run();
  return token;
}

export async function rotateRefreshToken(
  db: D1Database, oldToken: string
): Promise<{ userId: string; newToken: string } | null> {
  const hash = await sha256(oldToken);
  const row = await db.prepare(
    `SELECT user_id FROM refresh_tokens
     WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?`
  ).bind(hash, new Date().toISOString()).first();
  if (!row) return null;
  // revoke old, issue new
  await db.prepare(
    `UPDATE refresh_tokens SET revoked_at = ? WHERE token_hash = ?`
  ).bind(new Date().toISOString(), hash).run();
  const newToken = await createRefreshToken(db, row.user_id);
  return { userId: row.user_id, newToken };
}

export async function revokeRefreshToken(db: D1Database, token: string): Promise<void> {
  const hash = await sha256(token);
  await db.prepare(
    `UPDATE refresh_tokens SET revoked_at = ? WHERE token_hash = ?`
  ).bind(new Date().toISOString(), hash).run();
}
```

Rutas nuevas:

```
POST /api/auth/refresh   { refreshToken } -> { token, refreshToken? } (rota)
POST /api/auth/logout    { refreshToken } -> revoca
DELETE /api/auth/sessions (auth)          -> revoca todos del user
```

Login/register ahora devuelven `{ token, refreshToken, user }`.

### 1.6 Middleware chain en router

```typescript
// en el fetch handler
async function handle(request: Request, env: Env) {
  const url = new URL(request.url);

  if (request.method === "OPTIONS") {
    return new Response(null, { status: 204, headers: corsHeaders(request) });
  }

  // rate limit (excepto health)
  if (url.pathname !== "/api/health") {
    const ok = await enforceRateLimit(request, env);
    if (!ok) {
      return json({ error: "rate_limited" }, 429);
    }
  }

  const match = router.match(request.method, url.pathname);
  if (!match) return json({ error: "not_found" }, 404);

  try {
    let user: User | undefined;
    if (match.route.authed) {
      user = await requireUser(request, env);
    }
    const res = await match.route.handler({ request, url, env, user: user! }, match.params);
    // attach CORS
    const headers = new Headers(res.headers);
    for (const [k, v] of Object.entries(corsHeaders(request))) headers.set(k, v);
    return new Response(res.body, { status: res.status, headers });
  } catch (err) {
    if (err instanceof ApiError) {
      const res = json({ error: err.code }, err.status);
      const headers = new Headers(res.headers);
      for (const [k, v] of Object.entries(corsHeaders(request))) headers.set(k, v);
      return new Response(res.body, { status: res.status, headers });
    }
    console.error("unhandled error:", err);
    return json({ error: "internal_error" }, 500);
  }
}
```

## Cambios al cliente (Fase 1)

| Cambio | Archivo |
|---|---|
| Guardar refreshToken | `crates/lumen-core/src/auth.rs` |
| On 401 -> refresh -> retry (1 vez) | `crates/lumen-core/src/api.rs` |
| Logout llama `POST /api/auth/logout` | `apps/lumen-slint/src/controller.rs` |
| Acceso token expira en 1h -> refresco automatico | `crates/lumen-core/src/api.rs` |

## Aceptacion

- [ ] `pnpm smoke` pasa (actualizado con refresh flow)
- [ ] 11 requests rapidos a login -> 429 en el 11
- [ ] CORS: browser con origin no permitido -> sin headers CORS
- [ ] Body > 1 MB -> 413
- [ ] `GET /api/health` -> 200 `{ status: "ok" }`
- [ ] Access token expirado -> 401 -> refresh -> 200
- [ ] Refresh token reusado -> 401 (rotation)
- [ ] Logout revoca -> refresh con ese token -> 401
- [ ] Budget: sin cambios (rate limit usa Cache API, gratis)

## Riesgos

| Riesgo | Mitigacion |
|---|---|
| Cache API eviction agresiva | Aceptable: rate limit se reinicia, nunca se relaja |
| Clock skew en expiracion | Usar timestamps del server (ISO) consistentes |
| Cliente nativo sin Origin bloqueado | No aplica CORS sin Origin — JWT protege |
