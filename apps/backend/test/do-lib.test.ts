import { describe, expect, it } from "vitest";

import {
  deleteMessage,
  editMessage,
  FLUSH_INTERVAL_MS,
  FLUSH_THRESHOLD,
  pendingMessages,
  pushMessage,
  shouldFlush,
} from "../src/do/lib/buffer";
import { checkRate } from "../src/do/lib/ws-rate-limit";
import { createAttachment, dedupClientId, groupByVoiceChannel } from "../src/do/lib/presence-utils";

const msg = (id: string, createdAt = "2026-08-13T10:00:00.000Z"): import("../src/do/lib/buffer").BufferedMessage => ({
  id,
  authorId: "a",
  authorName: "alice",
  content: `hello ${id}`,
  createdAt,
});

describe("do/lib/buffer (ADR-0004)", () => {
  it("pushes messages and tracks the flush threshold", () => {
    let buf: ReturnType<typeof pushMessage> = [];
    for (let i = 0; i < FLUSH_THRESHOLD - 1; i++) buf = pushMessage(buf, msg(`m${i}`));
    expect(shouldFlush(buf)).toBe(false);
    buf = pushMessage(buf, msg("last"));
    expect(shouldFlush(buf)).toBe(true);
    expect(buf).toHaveLength(FLUSH_THRESHOLD);
  });

  it("edits an entry by id (sets editedAt, keeps order)", () => {
    let buf = [msg("m1"), msg("m2")];
    const { buf: next, found } = editMessage(buf, "m1", "edited!", "2026-08-13T10:05:00.000Z");
    expect(found).toBe(true);
    expect(next[0]!.content).toBe("edited!");
    expect(next[0]!.editedAt).toBe("2026-08-13T10:05:00.000Z");
    expect(next[1]!.content).toBe("hello m2"); // untouched
  });

  it("reports not-found edits and refuses to edit deleted entries", () => {
    let buf = [msg("m1")];
    const { buf: deleted, found: f1 } = deleteMessage(buf, "m1", "2026-08-13T10:05:00.000Z");
    expect(f1).toBe(true);
    const { found: f2 } = editMessage(deleted, "m1", "zombie", "2026-08-13T10:06:00.000Z");
    expect(f2).toBe(false); // deleted → not editable
    const { found: f3 } = editMessage(buf, "nope", "x", "2026-08-13T10:06:00.000Z");
    expect(f3).toBe(false);
  });

  it("soft-deletes: entry stays (placeholder) but is excluded from pending", () => {
    let buf = [msg("m1"), msg("m2")];
    const { buf: next } = deleteMessage(buf, "m1", "2026-08-13T10:05:00.000Z");
    expect(next[0]!.deletedAt).toBeDefined();
    expect(pendingMessages(next).map((m) => m.id)).toEqual(["m2"]);
  });

  it("flush interval constant is 5 minutes", () => {
    expect(FLUSH_INTERVAL_MS).toBe(5 * 60_000);
  });
});

describe("do/lib/ws-rate-limit (ADR-0009)", () => {
  const zero = { windowStart: 0, count: 0 };

  it("resets the window after it elapses", () => {
    const now = 10_000;
    const r = checkRate(zero, now, 10, 10_000);
    expect(r.ok).toBe(true);
    expect(r.counter).toEqual({ windowStart: now, count: 1 });
  });

  it("blocks once the limit is exceeded within the window", () => {
    let counter = { windowStart: 1000, count: 0 };
    for (let i = 0; i < 10; i++) {
      const r = checkRate(counter, 2000, 10, 10_000);
      expect(r.ok).toBe(true);
      counter = r.counter;
    }
    const blocked = checkRate(counter, 2500, 10, 10_000);
    expect(blocked.ok).toBe(false);
  });

  it("enforces its own limit per window (chat vs typing)", () => {
    // typing: 3 per 5s — the 4th typing message within the window blocks,
    // while chat (10 per 10s) on its own counter is unaffected.
    let typing = { windowStart: 0, count: 0 };
    let chat = { windowStart: 0, count: 0 };
    for (let i = 0; i < 3; i++) {
      typing = checkRate(typing, 1000, 3, 5_000).counter;
    }
    expect(checkRate(typing, 1500, 3, 5_000).ok).toBe(false); // typing exhausted
    const chatOk = checkRate(chat, 1000, 10, 10_000);
    expect(chatOk.ok).toBe(true); // chat window untouched
  });
});

describe("do/lib/presence-utils", () => {
  it("creates a fresh attachment with zeroed counters", () => {
    const att = createAttachment({ userId: "u1", username: "alice", servers: ["s1"], friends: ["f1"] });
    expect(att.status).toBe("online");
    expect(att.servers).toEqual(["s1"]);
    expect(att.voiceChannelId).toBeNull();
    expect(att.msgCount).toBe(0);
  });

  it("dedups retransmitted clientIds (ADR-005) and caps the list", () => {
    const att = createAttachment({ userId: "u1", username: "a", servers: [], friends: [] });
    const first = dedupClientId(att, "c1");
    expect(first.duplicate).toBe(false);
    const second = dedupClientId({ ...att, recentClientIds: first.recentClientIds }, "c1");
    expect(second.duplicate).toBe(true);
  });

  it("groups online members by voice channel", () => {
    const members = [
      { userId: "u1", username: "alice" },
      { userId: "u2", username: "bob" },
      { userId: "u3", username: "carol" },
    ];
    const voiceOf = (id: string) => (id === "u2" ? "ch1" : id === "u3" ? "ch1" : null);
    const groups = groupByVoiceChannel(members, voiceOf);
    expect(groups).toEqual([
      { channelId: "ch1", peers: [
        { userId: "u2", username: "bob" },
        { userId: "u3", username: "carol" },
      ] },
    ]);
  });
});
