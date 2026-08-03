import { describe, expect, it } from "vitest";

import { hashPassword, signToken, verifyPassword, verifyToken } from "../src/auth";
import {
  generateInviteCode,
  validateChannelKind,
  validateChannelName,
  validateContent,
  validatePassword,
  validateServerName,
  validateUsername,
} from "../src/validation";

const SECRET = "a".repeat(64);

async function signRaw(payload: object, secret: string): Promise<string> {
  const enc = new TextEncoder();
  const b64 = (bytes: Uint8Array) => Buffer.from(bytes).toString("base64url");
  const header = b64(enc.encode(JSON.stringify({ alg: "HS256", typ: "JWT" })));
  const body = b64(enc.encode(JSON.stringify(payload)));
  const key = await crypto.subtle.importKey(
    "raw",
    enc.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign("HMAC", key, enc.encode(`${header}.${body}`));
  return `${header}.${body}.${b64(new Uint8Array(sig))}`;
}

describe("password hashing (PBKDF2-SHA256)", () => {
  it("roundtrips the correct password and rejects wrong ones", async () => {
    const { salt, hash } = await hashPassword("correct horse battery staple");
    expect(salt).toMatch(/^[0-9a-f]{32}$/); // 16-byte salt hex
    expect(hash).toMatch(/^[0-9a-f]{64}$/); // 32-byte SHA-256 hex
    expect(await verifyPassword("correct horse battery staple", `${salt}:${hash}`)).toBe(true);
    expect(await verifyPassword("wrong password", `${salt}:${hash}`)).toBe(false);
  });

  it("uses a fresh random salt per hash", async () => {
    const a = await hashPassword("same-password");
    const b = await hashPassword("same-password");
    expect(a.salt).not.toBe(b.salt);
    expect(a.hash).not.toBe(b.hash);
  });

  it("rejects malformed stored values", async () => {
    expect(await verifyPassword("x", "not-a-valid-format")).toBe(false);
    expect(await verifyPassword("x", "abc:def")).toBe(false); // wrong lengths
  });
});

describe("JWT (HMAC-SHA256)", () => {
  it("signs and verifies with a 7-day expiry", async () => {
    const token = await signToken("user-123", SECRET);
    expect(token.split(".")).toHaveLength(3);
    const payload = await verifyToken(token, SECRET);
    expect(payload.sub).toBe("user-123");
    expect(payload.iat).toBeGreaterThan(0);
    expect(payload.exp - payload.iat).toBe(7 * 24 * 3600);
  });

  it("rejects tampered signatures", async () => {
    const token = await signToken("user-123", SECRET);
    const [h, p] = token.split(".");
    await expect(verifyToken(`${h}.${p}.AAAA`, SECRET)).rejects.toThrow();
    await expect(verifyToken(`${h}.${"bad"}.${token.split(".")[2]}`, SECRET)).rejects.toThrow();
  });

  it("rejects tokens signed with a different secret", async () => {
    const token = await signToken("user-123", "b".repeat(64));
    await expect(verifyToken(token, SECRET)).rejects.toThrow();
  });

  it("rejects expired tokens", async () => {
    const now = Math.floor(Date.now() / 1000);
    const expired = await signRaw({ sub: "user-123", iat: now - 100, exp: now - 10 }, SECRET);
    await expect(verifyToken(expired, SECRET)).rejects.toThrow("expired");
  });

  it("rejects malformed tokens", async () => {
    await expect(verifyToken("not-a-jwt", SECRET)).rejects.toThrow();
    await expect(verifyToken("a.b", SECRET)).rejects.toThrow();
  });
});

describe("validation helpers", () => {
  it("username: 3-32 chars [A-Za-z0-9_]", () => {
    expect(validateUsername("abc_123")).toBe(true);
    expect(validateUsername("a")).toBe(false);
    expect(validateUsername("ab")).toBe(false);
    expect(validateUsername("a".repeat(33))).toBe(false);
    expect(validateUsername("bad-name")).toBe(false);
    expect(validateUsername("with space")).toBe(false);
    expect(validateUsername("café")).toBe(false);
    expect(validateUsername(42)).toBe(false);
  });

  it("password: at least 8 chars", () => {
    expect(validatePassword("12345678")).toBe(true);
    expect(validatePassword("short")).toBe(false);
    expect(validatePassword(42)).toBe(false);
  });

  it("content: 1..2000 chars", () => {
    expect(validateContent("hi")).toBe(true);
    expect(validateContent("")).toBe(false);
    expect(validateContent("x".repeat(2000))).toBe(true);
    expect(validateContent("x".repeat(2001))).toBe(false);
  });

  it("server and channel names", () => {
    expect(validateServerName("Lumen Lounge")).toBe(true);
    expect(validateServerName("   ")).toBe(false);
    expect(validateServerName("")).toBe(false);
    expect(validateChannelName("general")).toBe(true);
    expect(validateChannelName("")).toBe(false);
  });

  it("channel kind", () => {
    expect(validateChannelKind("text")).toBe(true);
    expect(validateChannelKind("voice")).toBe(true);
    expect(validateChannelKind("video")).toBe(false);
  });

  it("invite codes are 8 url-safe chars and vary", () => {
    const a = generateInviteCode();
    expect(a).toMatch(/^[A-Za-z0-9]{8}$/);
    expect(generateInviteCode()).not.toBe(a);
  });
});
