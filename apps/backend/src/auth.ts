import { ApiError } from "./router";

/**
 * Password hashing: PBKDF2-SHA256, 16-byte random salt.
 * Stored as `saltHex:hashHex` (both hex). Runs on crypto.subtle so it is
 * available in both the Worker runtime and Node (vitest).
 *
 * Iterations = 100_000: the Workers runtime (workerd) rejects PBKDF2 with
 * more than 100k iterations ("iteration counts above 100000 are not
 * supported") — 210k (the OWASP-ish default) throws in production while
 * passing under miniflare. 100k is the platform maximum; measured ~0ms.
 */
const PBKDF2_ITERATIONS = 100_000;
const SALT_BYTES = 16;
const KEY_BYTES = 32; // SHA-256 output size

/** Access token TTL (ADR-0007): short-lived, memory-only on the client. */
export const ACCESS_TTL_SECONDS = 3600; // 1h
/** Refresh token TTL (ADR-0007): opaque, revocable, persisted by the client. */
export const REFRESH_TTL_SECONDS = 30 * 24 * 60 * 60; // 30 days

const enc = new TextEncoder();

function bytesToHex(bytes: Uint8Array): string {
  let out = "";
  for (const b of bytes) out += b.toString(16).padStart(2, "0");
  return out;
}

function hexToBytes(hex: string): Uint8Array {
  const bytes = new Uint8Array(hex.length / 2);
  for (let i = 0; i < bytes.length; i++) {
    bytes[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

function bytesToBase64Url(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function base64UrlToBytes(s: string): Uint8Array {
  const b64 = s.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (s.length % 4)) % 4);
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

async function deriveBits(password: string, salt: Uint8Array): Promise<Uint8Array> {
  const keyMaterial = await crypto.subtle.importKey("raw", enc.encode(password), "PBKDF2", false, [
    "deriveBits",
  ]);
  const bits = await crypto.subtle.deriveBits(
    { name: "PBKDF2", salt, iterations: PBKDF2_ITERATIONS, hash: "SHA-256" },
    keyMaterial,
    KEY_BYTES * 8,
  );
  return new Uint8Array(bits);
}

export async function hashPassword(password: string): Promise<{ salt: string; hash: string }> {
  const salt = crypto.getRandomValues(new Uint8Array(SALT_BYTES));
  const hash = await deriveBits(password, salt);
  return { salt: bytesToHex(salt), hash: bytesToHex(hash) };
}

export async function verifyPassword(password: string, stored: string): Promise<boolean> {
  const sep = stored.indexOf(":");
  if (sep === -1) return false;
  const saltHex = stored.slice(0, sep);
  const hashHex = stored.slice(sep + 1);
  if (saltHex.length !== SALT_BYTES * 2 || hashHex.length !== KEY_BYTES * 2) return false;
  try {
    const hash = await deriveBits(password, hexToBytes(saltHex));
    return bytesToHex(hash) === hashHex;
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// JWT (HMAC-SHA256, HS256). Header {alg,typ}, payload {sub,iat,exp}.
// ---------------------------------------------------------------------------

export interface JwtPayload {
  sub: string;
  iat: number;
  exp: number;
}

function hmacKey(secret: string): Promise<CryptoKey> {
  return crypto.subtle.importKey("raw", enc.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, [
    "sign",
    "verify",
  ]);
}

async function hmacSign(data: string, secret: string): Promise<string> {
  const key = await hmacKey(secret);
  const sig = await crypto.subtle.sign("HMAC", key, enc.encode(data));
  return bytesToBase64Url(new Uint8Array(sig));
}

export async function signToken(userId: string, secret: string): Promise<string> {
  const header = bytesToBase64Url(enc.encode(JSON.stringify({ alg: "HS256", typ: "JWT" })));
  const now = Math.floor(Date.now() / 1000);
  const payload = bytesToBase64Url(
    enc.encode(JSON.stringify({ sub: userId, iat: now, exp: now + ACCESS_TTL_SECONDS })),
  );
  const signingInput = `${header}.${payload}`;
  return `${signingInput}.${await hmacSign(signingInput, secret)}`;
}

export async function verifyToken(token: string, secret: string): Promise<JwtPayload> {
  const parts = token.split(".");
  if (parts.length !== 3) throw new Error("malformed token");
  const headerB64 = parts[0]!;
  const payloadB64 = parts[1]!;
  const signature = parts[2]!;
  const signingInput = `${headerB64}.${payloadB64}`;
  const key = await hmacKey(secret);
  const valid = await crypto.subtle.verify("HMAC", key, base64UrlToBytes(signature), enc.encode(signingInput));
  if (!valid) throw new Error("bad signature");
  const payload = JSON.parse(new TextDecoder().decode(base64UrlToBytes(payloadB64))) as Partial<JwtPayload>;
  if (typeof payload.sub !== "string" || typeof payload.exp !== "number") {
    throw new Error("bad payload");
  }
  if (payload.exp <= Math.floor(Date.now() / 1000)) throw new Error("token expired");
  return { sub: payload.sub, iat: typeof payload.iat === "number" ? payload.iat : 0, exp: payload.exp };
}

/** AUTH_SECRET guard — clear 500 instead of a confusing crypto failure. */
export function getSecret(env: Env): string {
  if (!env.AUTH_SECRET) {
    throw new ApiError(500, "server_misconfigured", "AUTH_SECRET is not set");
  }
  return env.AUTH_SECRET;
}

// ---------------------------------------------------------------------------
// Refresh tokens (ADR-0007): opaque 30d tokens, SHA-256 hashed in D1,
// rotated on every use (reuse = 401) and revocable (logout / sessions).
// ---------------------------------------------------------------------------

async function sha256Hex(value: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", enc.encode(value));
  return bytesToHex(new Uint8Array(digest));
}

/** Insert a refresh token row; returns the raw token (never stored). */
export async function createRefreshToken(
  db: D1Database,
  userId: string,
): Promise<string> {
  // Two UUIDs = 256 bits of entropy; opaque (no user info, no signature).
  const token = crypto.randomUUID() + crypto.randomUUID();
  const hash = await sha256Hex(token);
  await db
    .prepare(
      `INSERT INTO refresh_tokens (token_hash, user_id, expires_at)
       VALUES (?, ?, ?)`,
    )
    .bind(hash, userId, new Date(Date.now() + REFRESH_TTL_SECONDS * 1000).toISOString())
    .run();
  return token;
}

/**
 * Rotate a refresh token: revoke the presented one, issue a new one for the
 * same user. Returns null when the token is invalid (unknown, revoked, or
 * expired) — the caller answers 401. Reuse of an already-rotated token hits
 * the revoked_at check and fails, which is the rotation-compromise signal.
 */
export async function rotateRefreshToken(
  db: D1Database,
  oldToken: string,
): Promise<{ userId: string; newToken: string } | null> {
  const hash = await sha256Hex(oldToken);
  const row = await db
    .prepare(
      `SELECT user_id FROM refresh_tokens
       WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?`,
    )
    .bind(hash, new Date().toISOString())
    .first() as { user_id: string } | null;
  if (!row) return null;
  await db
    .prepare(`UPDATE refresh_tokens SET revoked_at = ? WHERE token_hash = ?`)
    .bind(new Date().toISOString(), hash)
    .run();
  const newToken = await createRefreshToken(db, row.user_id);
  return { userId: row.user_id, newToken };
}

/** Revoke one refresh token (logout). Idempotent. */
export async function revokeRefreshToken(db: D1Database, token: string): Promise<void> {
  const hash = await sha256Hex(token);
  await db
    .prepare(`UPDATE refresh_tokens SET revoked_at = ? WHERE token_hash = ? AND revoked_at IS NULL`)
    .bind(new Date().toISOString(), hash)
    .run();
}

/** Revoke every live refresh token of a user (logout all / account delete). */
export async function revokeAllSessions(db: D1Database, userId: string): Promise<void> {
  await db
    .prepare(`UPDATE refresh_tokens SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL`)
    .bind(new Date().toISOString(), userId)
    .run();
}
