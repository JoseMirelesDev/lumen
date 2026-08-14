/**
 * Secret env vars injected from .dev.vars locally (wrangler secret put in prod).
 * `wrangler types` regenerates worker-configuration.d.ts; this file merges the
 * secret contract into the global `Env` interface without touching generated code.
 */
declare global {
  interface Env extends __BaseEnv_Env {
    /** HMAC key for JWTs (32+ random bytes hex). Required for auth. */
    AUTH_SECRET: string;
    /** Cloudflare Realtime TURN key id — optional; STUN-only fallback when absent. */
    REALTIME_TURN_KEY_ID?: string;
    /** Cloudflare API token with Realtime permissions — optional. */
    REALTIME_API_TOKEN?: string;
    /** Local dev/test only — disables the Cache API rate limiter. NEVER set in production. */
    LUMEN_RATE_LIMIT_DISABLED?: string;
    /** OAuth (Fase 4): provider client credentials (wrangler secret put). */
    GOOGLE_CLIENT_ID?: string;
    GOOGLE_CLIENT_SECRET?: string;
    GITHUB_CLIENT_ID?: string;
    GITHUB_CLIENT_SECRET?: string;
    /** Base URL of the OAuth callback, e.g. https://api.dominio.com/api/oauth. */
    OAUTH_CALLBACK_URL?: string;
    /** Web client origin for post-OAuth redirect, e.g. https://app.dominio.com. */
    WEB_CLIENT_URL?: string;
  }
}

export {};
