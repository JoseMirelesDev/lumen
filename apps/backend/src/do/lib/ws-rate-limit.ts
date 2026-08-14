/**
 * Pure sliding-window rate limiting for WebSocket messages (ADR-0009, P3 del
 * review). Per-socket counters live in the socket attachment; this module is
 * the pure math so it is unit-testable without a DO runtime.
 *
 * The DO is single-threaded per instance, so a check+increment is atomic in
 * practice even though the function is pure.
 */

export interface WindowCounter {
  windowStart: number; // epoch ms when the current window opened
  count: number; // requests in the current window
}

export interface RateCheck {
  ok: boolean;
  /** The counter to persist back into the attachment (window may have reset). */
  counter: WindowCounter;
}

/**
 * Check + advance a sliding window. When `now - windowStart > windowMs` the
 * window resets (count = 1). Otherwise count increments; `ok` is false when
 * the increment would exceed `limit`.
 */
export function checkRate(
  counter: WindowCounter,
  now: number,
  limit: number,
  windowMs: number,
): RateCheck {
  // Fresh counter (never used) or expired window → start a new window now.
  if (counter.count === 0 || now - counter.windowStart >= windowMs) {
    return { ok: true, counter: { windowStart: now, count: 1 } };
  }
  const count = counter.count + 1;
  return { ok: count <= limit, counter: { windowStart: counter.windowStart, count } };
}
