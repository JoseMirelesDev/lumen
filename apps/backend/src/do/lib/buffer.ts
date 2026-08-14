/**
 * Pure chat-buffer logic for the PresenceHubDO (ADR-0004, P3 del review).
 * No Durable Object runtime types here — plain functions over plain arrays,
 * unit-testable without miniflare.
 *
 * The buffer lives in `state.storage` under `buf:<channelId>`; the DO is the
 * single writer (single-threaded), so these operations are serialized.
 */

export interface BufferedMessage {
  id: string;
  authorId: string;
  authorName: string;
  content: string;
  createdAt: string;
  editedAt?: string | null;
  deletedAt?: string | null;
  replyTo?: string | null;
}

export const FLUSH_THRESHOLD = 50; // mensajes por block
export const FLUSH_INTERVAL_MS = 5 * 60_000; // flush por alarm si no se llega al umbral

export function pushMessage(buf: BufferedMessage[], msg: BufferedMessage): BufferedMessage[] {
  return [...buf, msg];
}

/** Edit an entry by id. Returns the new buffer + whether the id existed. */
export function editMessage(
  buf: BufferedMessage[],
  messageId: string,
  content: string,
  nowIso: string,
): { buf: BufferedMessage[]; found: boolean } {
  let found = false;
  const next = buf.map((m) => {
    if (m.id === messageId && !m.deletedAt) {
      found = true;
      return { ...m, content, editedAt: nowIso };
    }
    return m;
  });
  return { buf: next, found };
}

/** Soft-delete an entry by id (placeholder on read). */
export function deleteMessage(
  buf: BufferedMessage[],
  messageId: string,
  nowIso: string,
): { buf: BufferedMessage[]; found: boolean } {
  let found = false;
  const next = buf.map((m) => {
    if (m.id === messageId && !m.deletedAt) {
      found = true;
      return { ...m, deletedAt: nowIso };
    }
    return m;
  });
  return { buf: next, found };
}

export function shouldFlush(buf: BufferedMessage[]): boolean {
  return buf.length >= FLUSH_THRESHOLD;
}

/** Messages still pending (never flushed), newest last. */
export function pendingMessages(buf: BufferedMessage[]): BufferedMessage[] {
  return buf.filter((m) => !m.deletedAt);
}
