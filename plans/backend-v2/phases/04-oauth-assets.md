# Fase 4 — OAuth y assets

Prioridad: **MEDIA** | Dependencias: Fase 1 (security base) | Duracion estimada: 3-4 dias

## Objetivo

Login con Google/GitHub, avatares y server icons en R2, y deeplinks de
autenticacion para el cliente nativo.

## Archivos

| Archivo | Cambio |
|---|---|
| `src/auth.ts` | OAuth exchange helpers |
| `src/index.ts` | Rutas /api/oauth/* + /api/assets/* + /api/me/avatar |
| `src/db.ts` | Queries OAuth + avatar |
| `migrations/0004_oauth.sql` | Columnas + oauth_states |
| `wrangler.toml` | R2 binding |
| `src/env.d.ts` | Tipos R2 + OAuth secrets |
| `crates/lumen-core/src/auth.rs` | Deep link handling |
| `apps/lumen-slint/src/main.rs` | Protocolo lumen:// |
| `apps/lumen-slint/ui/` | Login OAuth buttons, avatar display |

## Tareas

### 4.1 Schema: migration 0004_oauth.sql

```sql
ALTER TABLE users ADD COLUMN email TEXT;
ALTER TABLE users ADD COLUMN oauth_provider TEXT;
ALTER TABLE users ADD COLUMN oauth_id TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_oauth
  ON users(oauth_provider, oauth_id) WHERE oauth_provider IS NOT NULL;

CREATE TABLE IF NOT EXISTS oauth_states (
  state TEXT PRIMARY KEY,
  expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_oauth_states_expiry ON oauth_states(expires_at);
```

### 4.2 Rutas OAuth

```typescript
const OAUTH_PROVIDERS = {
  google: {
    authUrl: "https://accounts.google.com/o/oauth2/v2/auth",
    tokenUrl: "https://oauth2.googleapis.com/token",
    userInfoUrl: "https://www.googleapis.com/oauth2/v2/userinfo",
    scope: "openid email profile",
  },
  github: {
    authUrl: "https://github.com/login/oauth/authorize",
    tokenUrl: "https://github.com/login/oauth/access_token",
    userInfoUrl: "https://api.github.com/user",
    scope: "user:email",
  },
} as const;

router.get("/api/oauth/:provider", false, async (ctx, params) => {
  const provider = OAUTH_PROVIDERS[params.provider];
  if (!provider) throw new ApiError(404, "not_found");

  const state = crypto.randomUUID();
  await ctx.env.LUMEN_D1.prepare(
    "INSERT INTO oauth_states (state, expires_at) VALUES (?, ?)"
  ).bind(state, Date.now() + 600_000).run();

  const params_ = new URLSearchParams({
    client_id: ctx.env[`${params.provider.toUpperCase()}_CLIENT_ID`],
    redirect_uri: `${ctx.env.OAUTH_CALLBACK_URL}/${params.provider}`,
    response_type: "code",
    scope: provider.scope,
    state,
  });
  return Response.redirect(`${provider.authUrl}?${params_}`, 302);
});

router.get("/api/oauth/:provider/callback", false, async (ctx, params) => {
  const code = ctx.url.searchParams.get("code");
  const state = ctx.url.searchParams.get("state");
  if (!code || !state) throw new ApiError(400, "missing_params");

  // state anti-CSRF (delete + check en una op)
  const row = await ctx.env.LUMEN_D1.prepare(
    "DELETE FROM oauth_states WHERE state = ? AND expires_at > ? RETURNING state"
  ).bind(state, Date.now()).first();
  if (!row) throw new ApiError(400, "invalid_state");

  const provider = OAUTH_PROVIDERS[params.provider];
  // exchange code -> access token
  const tokenRes = await fetch(provider.tokenUrl, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded",
               accept: "application/json" },
    body: new URLSearchParams({
      client_id: ctx.env[`${params.provider.toUpperCase()}_CLIENT_ID`],
      client_secret: ctx.env[`${params.provider.toUpperCase()}_CLIENT_SECRET`],
      code, redirect_uri: `${ctx.env.OAUTH_CALLBACK_URL}/${params.provider}`,
      grant_type: "authorization_code",
    }),
  });
  if (!tokenRes.ok) throw new ApiError(502, "oauth_exchange_failed");
  const { access_token } = await tokenRes.json();

  // fetch user info
  const infoRes = await fetch(provider.userInfoUrl, {
    headers: { authorization: `Bearer ${access_token}`,
               "user-agent": "lumen-backend" },
  });
  if (!infoRes.ok) throw new ApiError(502, "oauth_userinfo_failed");
  const info = await infoRes.json();

  const oauthId = String(info.id ?? info.sub);
  const email = info.email ?? `${oauthId}@${params.provider}.local`;
  const username = (info.name ?? info.login ?? info.email?.split("@")[0] ?? "user")
    .replace(/[^A-Za-z0-9_]/g, "_").slice(0, 32) || `user_${oauthId.slice(0, 6)}`;

  // find or create
  let user = await db.getUserByOAuth(ctx.env.LUMEN_D1, params.provider, oauthId);
  if (!user) {
    user = await db.createOAuthUser(ctx.env.LUMEN_D1, {
      id: crypto.randomUUID(), username, email,
      oauthProvider: params.provider, oauthId,
    });
  }

  const token = await auth.signToken(user.id, auth.getSecret(ctx.env));
  const refresh = await auth.createRefreshToken(ctx.env.LUMEN_D1, user.id);

  const client = ctx.url.searchParams.get("client") ?? "web";
  const base = client === "desktop" ? "lumen://auth/callback"
    : ctx.env.WEB_CLIENT_URL;
  return Response.redirect(
    `${base}?token=${token}&refreshToken=${refresh}`, 302);
});
```

Secrets nuevos:

```bash
wrangler secret put GOOGLE_CLIENT_ID
wrangler secret put GOOGLE_CLIENT_SECRET
wrangler secret put GITHUB_CLIENT_ID
wrangler secret put GITHUB_CLIENT_SECRET
wrangler secret put OAUTH_CALLBACK_URL   # https://api.dominio.com/api/oauth
wrangler secret put WEB_CLIENT_URL       # https://app.dominio.com
```

### 4.3 R2 assets

```toml
# wrangler.toml
[[r2_buckets]]
binding = "LUMEN_R2"
bucket_name = "lumen-assets"
```

```typescript
// upload avatar (authed, 5 MB max)
router.put("/api/me/avatar", true, async (ctx) => {
  const size = Number(ctx.request.headers.get("content-length") ?? 0);
  if (size > 5 * 1024 * 1024) throw new ApiError(413, "too_large");
  const blob = await ctx.request.arrayBuffer();
  const key = `avatars/${ctx.user.id}.png`;
  await ctx.env.LUMEN_R2.put(key, blob, {
    httpMetadata: { contentType: "image/png", cacheControl: "public, max-age=86400" },
  });
  await db.updateAvatar(ctx.env.LUMEN_D1, ctx.user.id, key);
  return json({ url: `/api/assets/${key}` });
});

// upload server icon (owner, 5 MB)
router.put("/api/servers/:id/icon", true, async (ctx, params) => {
  const server = await db.getServer(ctx.env.LUMEN_D1, params.id!);
  if (!server) throw new ApiError(404, "not_found");
  if (server.owner_id !== ctx.user.id) throw new ApiError(403, "forbidden");
  // ... same pattern, key = server-icons/<id>.png
});

// serve assets (public, cacheable)
router.get("/api/assets/:path+", false, async (ctx, params) => {
  const obj = await ctx.env.LUMEN_R2.get(params.path!);
  if (!obj) return json({ error: "not_found" }, 404);
  return new Response(obj.body, {
    headers: {
      "content-type": obj.httpMetadata?.contentType ?? "application/octet-stream",
      "cache-control": "public, max-age=86400",
      "etag": obj.httpEtag,
    },
  });
});
```

### 4.4 Deep links (cliente nativo)

```rust
// apps/lumen-slint/src/main.rs — registro del protocolo

// Linux (xdg-mime): el instalador registra:
//   lumen.desktop -> Exec=lumen %u
//   MimeType=x-scheme-handler/lumen;

// Al arrancar, parsear argv:
fn parse_deeplink(args: &[String]) -> Option<Deeplink> {
    let url = args.iter().find(|a| a.starts_with("lumen://"))?;
    let rest = url.strip_prefix("lumen://")?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let params: HashMap<String, String> = query.split('&').filter_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        Some((k.to_string(), urlencoding::decode(v).ok()?.to_string()))
    }).collect();
    match path {
        "auth/callback" => Some(Deeplink::AuthCallback {
            token: params.get("token")?.clone(),
            refreshToken: params.get("refreshToken").cloned(),
        }),
        "invite" => params.get("code").map(|c| Deeplink::Invite(c.clone())),
        _ => None,
    }
}

enum Deeplink {
    AuthCallback { token: String, refreshToken: Option<String> },
    Invite(String),
}
```

OAuth desde el cliente nativo:

```
1. Cliente abre browser externo: https://api.dominio.com/api/oauth/google?client=desktop
2. User consente en Google
3. Redirect a lumen://auth/callback?token=...&refreshToken=...
4. OS reabre la app con el deeplink
5. main.rs parsea, guarda token, cierra el flujo
```

Alternativa sin browser externo: WebView embebido (WebView2/WKWebView/gtk
webview) que intercepta `lumen://` navigation. Mas control, menos
dependencia del OS.

### 4.5 Cliente: UI

- Login screen: botones "Continuar con Google" / "Continuar con GitHub"
  debajo del form clasico
- Avatar en el header del user + en mensajes (si `avatar` presente)
- Server icon en la sidebar
- Settings: cambiar avatar (file picker -> PUT /api/me/avatar)

## Aceptacion

- [ ] Flujo completo OAuth Google: login -> consent -> callback -> token
- [ ] Flujo completo OAuth GitHub: idem
- [ ] State reusado -> 400 invalid_state
- [ ] Avatar upload -> visible en /api/assets/avatars/:id
- [ ] Avatar > 5 MB -> 413
- [ ] Deep link `lumen://auth/callback?token=...` reabre la app autenticada
- [ ] Budget: R2 < 1% ops, Worker +2k req/dia max

## Riesgos

| Riesgo | Mitigacion |
|---|---|
| Google/GitHub rate limits | Normal (bajo volumen); handle 429 del provider con retry |
| Username collisions OAuth | Append suffix numerico en create; el user puede cambiarlo en settings |
| WebView vs browser externo | Empezar con browser externo (simple), WebView como mejora |
| e-mail duplicado entre cuentas | email no es UNIQUE en v1 (solo informativo); documentar |
