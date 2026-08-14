import type {
  PresenceClientMessage,
  PresenceServerMessage,
  PresenceV2Status,
} from "@lumen/protocol";

import {
  deleteMessage as bufferDelete,
  editMessage as bufferEdit,
  FLUSH_INTERVAL_MS,
  FLUSH_THRESHOLD,
  pushMessage,
  type BufferedMessage,
} from "./lib/buffer";
import { createAttachment, dedupClientId, type PresenceAttachment } from "./lib/presence-utils";
import { checkRate } from "./lib/ws-rate-limit";

/**
 * PresenceHubDO — singleton Durable Object for global presence (ADR-003),
 * the chat buffer (ADR-004), WS chat mutations (ADR-0010) and DM signaling
 * relay (ADR-006).
 *
 * Hibernation contract (same as LumenChannelDO, verified by review):
 *  - per-socket state lives ONLY in the socket attachment;
 *  - routing is tag-based (`userId`, `s:<serverId>`, `c:<channelId>`) —
 *    never iterate all sockets for a lookup;
 *  - the only state.storage use is the chat buffer (`buf:<channelId>`);
 *  - NO instance fields carry state across events; the constructor re-runs
 *    on every wake;
 *  - NO timers/polling: the alarm fires the 5-min buffer flush.
 *
 * IMPORTANT (Hibernation gotcha, mirrored from ChannelDO.ts): inside a
 * WebSocket event handler, `getWebSockets()` WITHOUT tags is scoped to the
 * event socket — always pass the tag explicitly.
 */

const OPEN = 1; // WebSocket.OPEN

// Rate limits (ARCHITECTURE.md §5.1, WS scope)
const CHAT_LIMIT = 10;
const CHAT_WINDOW = 10_000; // 10s
const TYPING_LIMIT = 3;
const TYPING_WINDOW = 5_000; // 5s
const VOICE_JOIN_LIMIT = 5;
const VOICE_JOIN_WINDOW = 60_000; // 1min
const DM_SIGNAL_LIMIT = 20;
const DM_SIGNAL_WINDOW = 60_000; // 1min

export class PresenceHubDO {
  private state: DurableObjectState;
  private env: Env;

  constructor(state: DurableObjectState, env: Env) {
    this.state = state;
    this.env = env;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);

    // Buffer probe for message pagination (Worker → DO, read-only).
    if (url.pathname.startsWith("/buffer/")) {
      const channelId = url.pathname.slice("/buffer/".length);
      const buf = await this.state.storage.get<BufferedMessage[]>(`buf:${channelId}`);
      return new Response(JSON.stringify(buf ?? []), {
        headers: { "content-type": "application/json" },
      });
    }

    // Presence WS upgrade. The Worker validated the JWT and resolved the
    // member server + friend lists (see index.ts /api/presence) — the DO
    // trusts the query params because the fetch came from the Worker.
    const userId = url.searchParams.get("userId");
    const username = url.searchParams.get("username");
    const servers = (url.searchParams.get("servers") ?? "").split(",").filter(Boolean);
    const friends = (url.searchParams.get("friends") ?? "").split(",").filter(Boolean);
    if (!userId || !username) {
      return new Response(JSON.stringify({ error: "missing_user" }), { status: 400 });
    }

    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];
    this.state.acceptWebSocket(server, [userId, ...servers.map((s) => `s:${s}`)]);
    server.serializeAttachment(
      createAttachment({ userId, username, servers, friends }) satisfies PresenceAttachment,
    );

    // Fire-and-forget: notify online friends + build the ready snapshot.
    // Failures here must not break the 101 handshake.
    this.buildReadySnapshot(server).catch((e) => console.error("presence connect:", e));

    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    let msg: PresenceClientMessage;
    try {
      msg = JSON.parse(
        typeof message === "string" ? message : new TextDecoder().decode(message),
      ) as PresenceClientMessage;
    } catch {
      this.sendError(ws, "bad_message", "invalid JSON");
      return;
    }
    const att = ws.deserializeAttachment() as PresenceAttachment | null;
    if (!att) return;

    await this.handleMessage(ws, att, msg);
  }

  private async handleMessage(ws: WebSocket, att: PresenceAttachment, msg: PresenceClientMessage): Promise<void> {
    switch (msg.type) {
      case "ready": {
        // The DO sends the snapshot on connect already; this is a re-sync
        // request (client reconnected after a gap).
        await this.buildReadySnapshot(ws);
        break;
      }

      case "status": {
        att.status = msg.status;
        ws.serializeAttachment(att);
        this.broadcastToFriends(att, { type: "friend-status", userId: att.userId, status: msg.status });
        break;
      }

      case "voice-join": {
        if (!this.enforceRate(att, "voice", VOICE_JOIN_LIMIT, VOICE_JOIN_WINDOW, ws)) return;
        if (!att.servers.includes(msg.serverId)) {
          this.sendError(ws, "forbidden", "not a member");
          return;
        }
        // Re-validate membership (R4: closes the kick/ban gap on live WS).
        const member = await this.env.LUMEN_D1.prepare(
          "SELECT 1 FROM server_members WHERE server_id = ? AND user_id = ?",
        )
          .bind(msg.serverId, att.userId)
          .first()
          .catch(() => null);
        if (!member) {
          this.sendError(ws, "forbidden", "no longer a member");
          ws.close(4403, "kicked");
          return;
        }
        const previousServer = att.voiceServerId;
        att.voiceChannelId = msg.channelId;
        att.voiceServerId = msg.serverId;
        ws.serializeAttachment(att);
        if (previousServer && previousServer !== msg.serverId) {
          this.broadcastVoiceUpdate(previousServer, ws);
        }
        this.broadcastVoiceUpdate(msg.serverId, ws);
        break;
      }

      case "voice-leave": {
        const serverId = att.voiceServerId;
        att.voiceChannelId = null;
        att.voiceServerId = null;
        ws.serializeAttachment(att);
        if (serverId) this.broadcastVoiceUpdate(serverId, ws);
        break;
      }

      case "chat": {
        if (typeof msg.content !== "string" || msg.content.trim().length === 0) {
          this.sendError(ws, "bad_message", "content must be 1-2000 chars");
          return;
        }
        if (!this.enforceRate(att, "chat", CHAT_LIMIT, CHAT_WINDOW, ws)) return;
        if (!att.servers.includes(msg.serverId) && msg.serverId !== "") {
          this.sendError(ws, "forbidden", "not a member");
          return;
        }
        // Dedup retransmissions (client didn't get the ACK, ADR-005).
        const dedup = dedupClientId(att, msg.clientId);
        if (dedup.duplicate) return;
        att.recentClientIds = dedup.recentClientIds;
        ws.serializeAttachment(att);

        // Authoritative access + route: server member OR dm participant (R4
        // closes the kick/ban gap; 1 D1 read per message ≈ 1% read budget).
        const access = await this.channelAccess(msg.channelId, att.userId);
        if (!access.ok) {
          this.sendError(ws, "forbidden", "no longer a member");
          ws.close(4403, "kicked");
          return;
        }

        const message: BufferedMessage = {
          id: crypto.randomUUID(),
          authorId: att.userId,
          authorName: att.username,
          content: msg.content.slice(0, 2000),
          createdAt: new Date().toISOString(),
          ...(msg.replyTo ? { replyTo: msg.replyTo } : {}),
          ...(msg.attachmentUrl ? { attachmentUrl: msg.attachmentUrl.slice(0, 500) } : {}),
        };
        const key = `buf:${msg.channelId}`;
        // Atomic read-modify-write: concurrent WS events interleave across
        // awaits (input gate open), so a plain get+put would lose updates and
        // flushes could duplicate blocks. Storage transactions serialize on
        // the buffer key (single-writer per channel, ADR-0010).
        const bufLen = await this.state.storage.transaction<number>(async (txn) => {
          const buf = (await txn.get<BufferedMessage[]>(key)) ?? [];
          const next = pushMessage(buf, message);
          await txn.put(key, next);
          return next.length;
        });

        this.routeBroadcast(
          access,
          msg.channelId,
          { type: "chat", channelId: msg.channelId, message },
          ws,
        );
        ws.send(
          JSON.stringify({
            type: "chat-ack",
            clientId: msg.clientId,
            messageId: message.id,
            createdAt: message.createdAt,
          } satisfies PresenceServerMessage),
        );

        if (bufLen >= FLUSH_THRESHOLD) {
          await this.flushChannel(msg.channelId);
        } else if (!(await this.state.storage.getAlarm())) {
          await this.state.storage.setAlarm(Date.now() + FLUSH_INTERVAL_MS);
        }
        break;
      }

      case "chat-edit": {
        if (!att.servers.includes(msg.serverId) && msg.serverId !== "") {
          this.sendError(ws, "forbidden", "not a member");
          return;
        }
        // Author-only (ADR-0010). Buffer first, then flushed blocks (rewrite).
        const key = `buf:${msg.channelId}`;
        const buf = (await this.state.storage.get<BufferedMessage[]>(key)) ?? [];
        const entry = buf.find((m) => m.id === msg.messageId);
        let ok = false;
        if (entry) {
          if (entry.authorId !== att.userId) {
            this.sendError(ws, "bad_message", "message not found");
            return;
          }
          const edited = bufferEdit(buf, msg.messageId, msg.content.slice(0, 2000), new Date().toISOString());
          if (edited.found) {
            await this.state.storage.put(key, edited.buf);
            ok = true;
          }
        } else {
          ok = await this.mutateFlushedMessage(
            msg.channelId,
            msg.messageId,
            att.userId,
            (m) => ({ ...m, content: msg.content.slice(0, 2000), editedAt: new Date().toISOString() }),
          );
        }
        if (!ok) {
          this.sendError(ws, "bad_message", "message not found");
          return;
        }
        // Route the invalidation broadcast (R4: kick/ban also blocks edits).
        const access = await this.channelAccess(msg.channelId, att.userId);
        if (!access.ok) {
          this.sendError(ws, "forbidden", "no longer a member");
          ws.close(4403, "kicked");
          return;
        }
        ws.send(
          JSON.stringify({ type: "chat-edit-ack", clientId: msg.clientId, messageId: msg.messageId } satisfies PresenceServerMessage),
        );
        this.routeBroadcast(
          access,
          msg.channelId,
          {
            type: "chat-edited",
            channelId: msg.channelId,
            message: { id: msg.messageId, content: msg.content, editedAt: new Date().toISOString() },
          } satisfies PresenceServerMessage,
          ws,
        );
        break;
      }

      case "chat-delete": {
        if (!att.servers.includes(msg.serverId) && msg.serverId !== "") {
          this.sendError(ws, "forbidden", "not a member");
          return;
        }
        // Author-only (ADR-0010). Buffer first, then flushed blocks (rewrite).
        const key = `buf:${msg.channelId}`;
        const buf = (await this.state.storage.get<BufferedMessage[]>(key)) ?? [];
        const entry = buf.find((m) => m.id === msg.messageId);
        let ok = false;
        if (entry) {
          if (entry.authorId !== att.userId) {
            this.sendError(ws, "bad_message", "message not found");
            return;
          }
          const deleted = bufferDelete(buf, msg.messageId, new Date().toISOString());
          if (deleted.found) {
            await this.state.storage.put(key, deleted.buf);
            ok = true;
          }
        } else {
          ok = await this.mutateFlushedMessage(
            msg.channelId,
            msg.messageId,
            att.userId,
            (m) => ({ ...m, deletedAt: new Date().toISOString() }),
          );
        }
        if (!ok) {
          this.sendError(ws, "bad_message", "message not found");
          return;
        }
        const access = await this.channelAccess(msg.channelId, att.userId);
        if (!access.ok) {
          this.sendError(ws, "forbidden", "no longer a member");
          ws.close(4403, "kicked");
          return;
        }
        ws.send(
          JSON.stringify({ type: "chat-delete-ack", clientId: msg.clientId, messageId: msg.messageId } satisfies PresenceServerMessage),
        );
        this.routeBroadcast(
          access,
          msg.channelId,
          { type: "chat-deleted", channelId: msg.channelId, messageId: msg.messageId } satisfies PresenceServerMessage,
          ws,
        );
        break;
      }

      case "typing": {
        if (!this.enforceRate(att, "typing", TYPING_LIMIT, TYPING_WINDOW, ws, true)) return;
        const access = await this.channelAccess(msg.channelId, att.userId);
        if (!access.ok) return; // kicked member: drop silently
        this.routeBroadcast(
          access,
          msg.channelId,
          { type: "typing", channelId: msg.channelId, userId: att.userId } satisfies PresenceServerMessage,
          ws,
        );
        break;
      }

      case "subscribe": {
        // Tags are immutable after accept (workerd#958) — subscriptions live
        // in the attachment; broadcasts iterate the server tag and filter.
        if (!att.subscribedChannels.includes(msg.channelId)) {
          att.subscribedChannels.push(msg.channelId);
          ws.serializeAttachment(att);
        }
        // Ack: the sender must wait for subscribe-ack before expecting chat
        // broadcasts — otherwise a chat sent right after subscribe can be
        // filtered out (message ordering across sockets is not guaranteed).
        ws.send(
          JSON.stringify({ type: "subscribe-ack", channelId: msg.channelId } satisfies PresenceServerMessage),
        );
        break;
      }

      case "unsubscribe": {
        att.subscribedChannels = att.subscribedChannels.filter((c) => c !== msg.channelId);
        ws.serializeAttachment(att);
        break;
      }

      case "dm-signal": {
        if (!this.enforceRate(att, "dm", DM_SIGNAL_LIMIT, DM_SIGNAL_WINDOW, ws)) return;
        const target = this.state.getWebSockets(msg.to)[0];
        if (!target || target.readyState !== OPEN) {
          this.sendError(ws, "offline", "user offline");
          return;
        }
        const relay: PresenceServerMessage =
          msg.kind === "offer"
            ? { type: "dm-offer", from: att.userId, sdp: msg.sdp ?? "" }
            : msg.kind === "answer"
              ? { type: "dm-answer", from: att.userId, sdp: msg.sdp ?? "" }
              : { type: "dm-ice", from: att.userId, candidate: msg.candidate ?? null };
        target.send(JSON.stringify(relay));
        break;
      }

      case "reaction-toggle": {
        if (!att.servers.includes(msg.serverId) && msg.serverId !== "") {
          this.sendError(ws, "forbidden", "not a member");
          return;
        }
        const access = await this.channelAccess(msg.channelId, att.userId);
        if (!access.ok) {
          this.sendError(ws, "forbidden", "no longer a member");
          ws.close(4403, "kicked");
          return;
        }
        // Toggle (1 write) + broadcast to subscribers (Fase 6.1).
        const existing = await this.env.LUMEN_D1.prepare(
          "SELECT 1 FROM reactions WHERE message_id = ? AND user_id = ? AND emoji = ?",
        )
          .bind(msg.messageId, att.userId, msg.emoji.slice(0, 32))
          .first()
          .catch(() => null);
        let added: boolean;
        if (existing) {
          await this.env.LUMEN_D1.prepare(
            "DELETE FROM reactions WHERE message_id = ? AND user_id = ? AND emoji = ?",
          )
            .bind(msg.messageId, att.userId, msg.emoji.slice(0, 32))
            .run()
            .catch(() => {});
          added = false;
        } else {
          await this.env.LUMEN_D1.prepare(
            "INSERT INTO reactions (message_id, channel_id, user_id, emoji) VALUES (?, ?, ?, ?)",
          )
            .bind(msg.messageId, msg.channelId, att.userId, msg.emoji.slice(0, 32))
            .run()
            .catch(() => {});
          added = true;
        }
        this.routeBroadcast(
          access,
          msg.channelId,
          {
            type: "reaction",
            channelId: msg.channelId,
            messageId: msg.messageId,
            emoji: msg.emoji.slice(0, 32),
            userId: att.userId,
            added,
          } satisfies PresenceServerMessage,
          ws,
        );
        break;
      }

      case "ping": {
        ws.send(JSON.stringify({ type: "pong" } satisfies PresenceServerMessage));
        break;
      }

      default: {
        this.sendError(ws, "bad_message", "unknown type");
      }
    }
  }

  async webSocketClose(ws: WebSocket): Promise<void> {
    const att = ws.deserializeAttachment() as PresenceAttachment | null;
    if (!att) return;

    // Notify member servers (member-offline + voice occupancy recompute).
    for (const serverId of att.servers) {
      this.broadcastToTag(
        `s:${serverId}`,
        { type: "member-offline", serverId, userId: att.userId } satisfies PresenceServerMessage,
        ws,
      );
      if (att.voiceChannelId && att.voiceServerId === serverId) {
        this.broadcastVoiceUpdate(serverId, ws);
      }
    }
    // Notify online friends.
    this.broadcastToFriends(att, { type: "friend-offline", userId: att.userId });

    // last_seen — the single D1 write per disconnect (budgeted, BUDGET.md).
    await this.env.LUMEN_D1.prepare("UPDATE users SET last_seen = ? WHERE id = ?")
      .bind(new Date().toISOString(), att.userId)
      .run()
      .catch(() => {});
  }

  /** Alarm: flush every non-empty buffer (5-min cadence or threshold hit).
   *  Each flushChannel is a storage transaction — atomic against concurrent
   *  chat RMWs. */
  async alarm(): Promise<void> {
    const entries = await this.state.storage.list({ prefix: "buf:" });
    for (const [key, buf] of entries) {
      const messages = buf as BufferedMessage[];
      if (messages.length > 0) {
        await this.flushChannel(key.slice("buf:".length));
      }
    }
  }

  // -------------------------------------------------------------------------
  // Internals
  // -------------------------------------------------------------------------

  /** Notify online friends + send the `ready` snapshot to `ws`. */
  private async buildReadySnapshot(ws: WebSocket): Promise<void> {
    const att = ws.deserializeAttachment() as PresenceAttachment | null;
    if (!att) return;

    const onlineFriends: { userId: string; username: string; status: PresenceV2Status }[] = [];
    for (const friendId of att.friends) {
      const sockets = this.state.getWebSockets(friendId);
      if (sockets.length > 0) {
        const friendAtt = sockets[0]!.deserializeAttachment() as PresenceAttachment;
        onlineFriends.push({
          userId: friendId,
          username: friendAtt.username,
          status: friendAtt.status,
        });
        sockets[0]!.send(
          JSON.stringify({ type: "friend-online", userId: att.userId, username: att.username } satisfies PresenceServerMessage),
        );
      }
    }

    const servers: { serverId: string; onlineMembers: { userId: string; username: string }[]; voiceChannels: { channelId: string; peers: { userId: string; username: string }[] }[] }[] = [];
    for (const serverId of att.servers) {
      const members = this.state.getWebSockets(`s:${serverId}`);
      const onlineMembers: { userId: string; username: string }[] = [];
      const byVoice = new Map<string, { userId: string; username: string }[]>();
      for (const s of members) {
        if (s.readyState !== OPEN) continue;
        const a = s.deserializeAttachment() as PresenceAttachment;
        onlineMembers.push({ userId: a.userId, username: a.username });
        if (a.voiceChannelId) {
          const list = byVoice.get(a.voiceChannelId) ?? [];
          list.push({ userId: a.userId, username: a.username });
          byVoice.set(a.voiceChannelId, list);
        }
      }
      servers.push({
        serverId,
        onlineMembers,
        voiceChannels: [...byVoice.entries()].map(([channelId, peers]) => ({ channelId, peers })),
      });
      // Real-time member lists: tell the server's other online members.
      this.broadcastToTag(
        `s:${serverId}`,
        { type: "member-online", serverId, userId: att.userId, username: att.username } satisfies PresenceServerMessage,
        ws,
      );
    }

    ws.send(
      JSON.stringify({ type: "ready", onlineFriends, servers } satisfies PresenceServerMessage),
    );
  }

  /** Recompute a server's voice occupancy and broadcast to its members. */
  private broadcastVoiceUpdate(serverId: string, except?: WebSocket): void {
    const members = this.state.getWebSockets(`s:${serverId}`);
    const byVoice = new Map<string, { userId: string; username: string }[]>();
    for (const s of members) {
      if (s.readyState !== OPEN) continue;
      const a = s.deserializeAttachment() as PresenceAttachment;
      if (a.voiceChannelId) {
        const list = byVoice.get(a.voiceChannelId) ?? [];
        list.push({ userId: a.userId, username: a.username });
        byVoice.set(a.voiceChannelId, list);
      }
    }
    for (const [channelId, peers] of byVoice) {
      this.broadcastToTag(
        `s:${serverId}`,
        { type: "voice-update", serverId, channelId, peers } satisfies PresenceServerMessage,
        except,
      );
    }
  }

  /**
   * One-query channel access + routing info: server membership for server
   * channels, active dm_membership for DM channels (R4 re-validation on
   * chat). Returns the broadcast route: for DMs the other participant's
   * userId (tag lookup, O(1)); for server channels the serverId (tag scope).
   */
  private async channelAccess(
    channelId: string,
    userId: string,
  ): Promise<{ ok: boolean; kind: string; serverId: string | null; otherUserId: string | null }> {
    const row = (await this.env.LUMEN_D1.prepare(
      `SELECT c.kind, c.server_id,
         EXISTS(SELECT 1 FROM server_members sm WHERE sm.server_id = c.server_id AND sm.user_id = ?) AS is_server_member,
         EXISTS(SELECT 1 FROM dm_members dm WHERE dm.channel_id = c.id AND dm.user_id = ? AND dm.deleted_at IS NULL) AS is_dm_member,
         (SELECT o.user_id FROM dm_members o WHERE o.channel_id = c.id AND o.user_id != ? AND o.deleted_at IS NULL) AS other_user_id
       FROM channels c WHERE c.id = ?`,
    )
      .bind(userId, userId, userId, channelId)
      .first()
      .catch(() => null)) as {
      kind: string;
      server_id: string | null;
      is_server_member: number;
      is_dm_member: number;
      other_user_id: string | null;
    } | null;
    if (!row) return { ok: false, kind: "", serverId: null, otherUserId: null };
    if (row.kind === "dm") {
      return {
        ok: row.is_dm_member === 1,
        kind: row.kind,
        serverId: null,
        otherUserId: row.other_user_id,
      };
    }
    return {
      ok: row.is_server_member === 1,
      kind: row.kind,
      serverId: row.server_id,
      otherUserId: null,
    };
  }

  /**
   * Edit/delete a message that was already flushed to message_blocks
   * (ADR-0010): find the block (newest-first, bounded scan), mutate the JSON
   * entry, rewrite the block. The DO is single-threaded → serialized. Returns
   * false when the id isn't found or the caller isn't the author.
   */
  private async mutateFlushedMessage(
    channelId: string,
    messageId: string,
    userId: string,
    mutate: (m: BufferedMessage) => BufferedMessage,
  ): Promise<boolean> {
    const { results } = await this.env.LUMEN_D1.prepare(
      `SELECT id, messages FROM message_blocks
       WHERE channel_id = ? ORDER BY last_at DESC, id DESC LIMIT 20`,
    )
      .bind(channelId)
      .all<{ id: string; messages: string }>();
    for (const row of results) {
      const entries = JSON.parse(row.messages) as BufferedMessage[];
      const idx = entries.findIndex((m) => m.id === messageId);
      if (idx === -1) continue;
      if (entries[idx]!.authorId !== userId) return false;
      entries[idx] = mutate(entries[idx]!);
      await this.env.LUMEN_D1.prepare("UPDATE message_blocks SET messages = ? WHERE id = ?")
        .bind(JSON.stringify(entries), row.id)
        .run();
      return true;
    }
    return false;
  }

  /**
   * Flush a channel buffer to D1 as one packed block (ADR-0004). Two phases:
   * 1) atomically drain the buffer (storage transaction — serialized against
   *    concurrent chat RMWs on the same key, no interleaved duplicates);
   * 2) persist OUTSIDE the transaction (D1 calls inside a storage
   *    transaction destabilize miniflare/workerd). On D1 failure the drained
   *    messages are re-buffered so the next alarm retries.
   */
  private async flushChannel(channelId: string): Promise<void> {
    const key = `buf:${channelId}`;
    const buf = await this.state.storage.transaction<BufferedMessage[] | null>(async (txn) => {
      const cur = (await txn.get<BufferedMessage[]>(key)) ?? [];
      if (cur.length === 0) return null;
      await txn.delete(key);
      return cur;
    });
    if (!buf) return;
    try {
      await this.env.LUMEN_D1.prepare(
        `INSERT INTO message_blocks (id, channel_id, messages, count, first_at, last_at)
         VALUES (?, ?, ?, ?, ?, ?)`,
      )
        .bind(
          crypto.randomUUID(),
          channelId,
          JSON.stringify(buf),
          buf.length,
          buf[0]!.createdAt,
          buf[buf.length - 1]!.createdAt,
        )
        .run();
    } catch (e) {
      console.error("flush:", e);
      // Re-buffer to avoid loss; the alarm retries.
      await this.state.storage.transaction(async (txn) => {
        const cur = (await txn.get<BufferedMessage[]>(key)) ?? [];
        await txn.put(key, [...buf, ...cur]);
      });
    }
  }

  /**
   * Route a chat/typing/invalidation broadcast: DM → the other participant's
   * userId tag (O(1)); server channel → the server tag, filtered by the
   * receivers' attachment subscriptions (tags are immutable post-accept,
   * workerd#958 — subscriptions live in the attachment).
   */
  private routeBroadcast(
    route: { kind: string; serverId: string | null; otherUserId: string | null },
    channelId: string,
    msg: PresenceServerMessage,
    except?: WebSocket,
  ): void {
    if (route.kind === "dm") {
      if (route.otherUserId) this.broadcastToTag(route.otherUserId, msg, except);
      return;
    }
    if (route.serverId) {
      const payload = JSON.stringify(msg);
      for (const s of this.state.getWebSockets(`s:${route.serverId}`)) {
        if (s.readyState !== OPEN || s === except) continue;
        const a = s.deserializeAttachment() as PresenceAttachment | null;
        if (a && a.subscribedChannels.includes(channelId)) s.send(payload);
      }
    }
  }

  private broadcastToTag(tag: string, msg: PresenceServerMessage, except?: WebSocket): void {
    const payload = JSON.stringify(msg);
    for (const s of this.state.getWebSockets(tag)) {
      if (s.readyState === OPEN && s !== except) s.send(payload);
    }
  }

  private broadcastToFriends(att: PresenceAttachment, msg: PresenceServerMessage): void {
    const payload = JSON.stringify(msg);
    for (const friendId of att.friends) {
      for (const s of this.state.getWebSockets(friendId)) {
        if (s.readyState === OPEN) s.send(payload);
      }
    }
  }

  private sendError(ws: WebSocket, code: string, message: string): void {
    if (ws.readyState !== OPEN) return;
    ws.send(JSON.stringify({ type: "error", code, message } satisfies PresenceServerMessage));
  }

  /**
   * WS rate limit (ADR-0009): sliding window counters in the attachment.
   * `silent` drops without an error (typing is best-effort). The dev escape
   * hatch (LUMEN_RATE_LIMIT_DISABLED, same as the REST limiter) lets the
   * smoke suite burst messages to test flush/pagination.
   */
  private enforceRate(
    att: PresenceAttachment,
    kind: "chat" | "typing" | "voice" | "dm",
    limit: number,
    windowMs: number,
    ws: WebSocket,
    silent = false,
  ): boolean {
    if (this.env.LUMEN_RATE_LIMIT_DISABLED === "true") return true;
    const now = Date.now();
    let counter: { windowStart: number; count: number };
    let persist: (c: { windowStart: number; count: number }) => void;
    switch (kind) {
      case "chat":
        counter = { windowStart: att.msgWindowStart, count: att.msgCount };
        persist = (c) => {
          att.msgWindowStart = c.windowStart;
          att.msgCount = c.count;
        };
        break;
      case "typing":
        counter = { windowStart: att.typingWindowStart, count: att.typingCount };
        persist = (c) => {
          att.typingWindowStart = c.windowStart;
          att.typingCount = c.count;
        };
        break;
      case "voice":
        counter = { windowStart: att.voiceWindowStart, count: att.voiceCount };
        persist = (c) => {
          att.voiceWindowStart = c.windowStart;
          att.voiceCount = c.count;
        };
        break;
      default:
        counter = { windowStart: att.dmWindowStart, count: att.dmCount };
        persist = (c) => {
          att.dmWindowStart = c.windowStart;
          att.dmCount = c.count;
        };
        break;
    }
    const rl = checkRate(counter, now, limit, windowMs);
    persist(rl.counter);
    ws.serializeAttachment(att);
    if (!rl.ok && !silent) this.sendError(ws, "rate_limited", "slow down");
    return rl.ok;
  }
}
