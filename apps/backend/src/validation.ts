/**
 * Input validation helpers — shared by REST handlers and unit tests.
 * All validators return a type guard so callers get narrowed strings.
 */

export const USERNAME_RE = /^[A-Za-z0-9_]{3,32}$/;

export function validateUsername(value: unknown): value is string {
  return typeof value === "string" && USERNAME_RE.test(value);
}

export function validatePassword(value: unknown): value is string {
  return typeof value === "string" && value.length >= 8;
}

export function validateServerName(value: unknown): value is string {
  return typeof value === "string" && value.trim().length >= 1 && value.trim().length <= 100;
}

export function validateChannelName(value: unknown): value is string {
  return typeof value === "string" && value.trim().length >= 1 && value.trim().length <= 50;
}

export function validateChannelKind(value: unknown): value is "text" | "voice" {
  return value === "text" || value === "voice";
}

export function validateContent(value: unknown): value is string {
  return typeof value === "string" && value.length >= 1 && value.length <= 2000;
}

/** 8 random chars from crypto.getRandomValues (URL-safe alphabet). */
export function generateInviteCode(): string {
  const ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  const bytes = crypto.getRandomValues(new Uint8Array(8));
  let out = "";
  for (const b of bytes) out += ALPHABET[b % ALPHABET.length];
  return out;
}
