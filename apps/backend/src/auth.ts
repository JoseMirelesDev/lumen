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
const TOKEN_TTL_SECONDS = 7 * 24 * 60 * 60;

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
    enc.encode(JSON.stringify({ sub: userId, iat: now, exp: now + TOKEN_TTL_SECONDS })),
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
