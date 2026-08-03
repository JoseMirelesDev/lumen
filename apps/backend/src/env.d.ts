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
  }
}

export {};
