import type { ClientMessage, PeerInfo, ServerMessage } from "@lumen/protocol";

/**
 * LumenChannelDO — WebSocket Hibernation API from the first commit.
 *
 * One instance per channel (`lumen-<channelId>`), max 4 peers.
 *
 * Hibernation contract (verified by review):
 *  - sockets are accepted via `state.acceptWebSocket(server, [channelId, userId])`;
 *  - per-socket state lives in the socket attachment (`serializeAttachment`);
 *  - the peer list is persisted to `state.storage` key `peers` on every join/leave;
 *  - NO instance fields carry state across events (the object is re-constructed
 *    on wake — everything is re-derived from tags/attachments/storage);
 *  - NO timers, NO setInterval, NO polling loops — the object returns from each
 *    event handler immediately and hibernates.
 */

interface SocketAttachment {
  peerId: string;
  userId: string;
  joined: boolean;
}

const MAX_PEERS = 4;
const PEERS_KEY = "peers";
const OPEN = 1; // WebSocket.OPEN

export class LumenChannelDO {
  private state: DurableObjectState;

  constructor(state: DurableObjectState, _env: Env) {
    this.state = state;
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const channelId = url.searchParams.get("channelId");
    const userId = url.searchParams.get("userId");
    if (!channelId || !userId) {
      return new Response(JSON.stringify({ error: "missing_channel_or_user" }), {
        status: 400,
        headers: { "content-type": "application/json" },
      });
    }

    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];

    const peerId = crypto.randomUUID();
    this.state.acceptWebSocket(server, [channelId, userId]);
    server.serializeAttachment({ peerId, userId, joined: false } satisfies SocketAttachment);

    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    let msg: ClientMessage;
    try {
      msg = JSON.parse(
        typeof message === "string" ? message : new TextDecoder().decode(message),
      ) as ClientMessage;
    } catch {
      this.sendError(ws, "bad_message", "invalid JSON");
      return;
    }

    const att = ws.deserializeAttachment() as SocketAttachment | null;
    if (!att) return; // e.g. a channel_full socket that was already closed

    switch (msg.type) {
      case "join": {
        if (att.joined) {
          this.sendError(ws, "bad_message", "already joined");
          return;
        }
        const [channelTag, userTag] = this.state.getTags(ws);
        if (msg.channelId !== channelTag) {
          this.sendError(ws, "bad_message", "channel mismatch");
          return;
        }
        if (msg.userId !== userTag) {
          this.sendError(ws, "unauthorized", "user mismatch");
          return;
        }
        att.joined = true;
        ws.serializeAttachment(att);

        let peers = (await this.state.storage.get<PeerInfo[]>(PEERS_KEY)) ?? [];
        // Prune ghosts: entries whose socket is gone (DO restarted under a
        // live peer map, or a socket died without a close event). Otherwise
        // late joiners are handed dead peer ids and relays fail forever.
        const live = peers.filter((p) => this.findLiveSocket(p.peerId, p.userId) !== null);
        if (live.length !== peers.length) {
          peers = live;
          await this.state.storage.put(PEERS_KEY, peers);
        }
        // Dedup: one connection per user. A stale socket for the same userId
        // (client died without a close event, or reconnected from elsewhere)
        // must not linger as a ghost peer — close it and replace the entry.
        // The old socket's webSocketClose will call removePeer, which is
        // idempotent once its peerId is gone from the map.
        const previous = peers.find((p) => p.userId === att.userId);
        console.log(`[do] join userId=${att.userId} previous=${previous?.peerId ?? "none"} peers=${peers.map((p) => p.userId.slice(0, 6)).join(",")}`);
        if (previous) {
          const old = this.findLiveSocket(previous.peerId, previous.userId);
          if (old && old !== ws) {
            try {
              old.close(4000, "replaced");
            } catch {
              /* already closed */
            }
          }
          peers = peers.filter((p) => p.userId !== att.userId);
          await this.state.storage.put(PEERS_KEY, peers);
          await this.broadcast(
            { type: "peer-left", peerId: previous.peerId } satisfies ServerMessage,
            ws,
          );
        }
        if (peers.length >= MAX_PEERS) {
          this.sendError(ws, "channel_full", "channel is full (max 4 peers)");
          ws.close(1013, "channel_full");
          return;
        }
        peers.push({ peerId: att.peerId, userId: att.userId, username: msg.username });
        await this.state.storage.put(PEERS_KEY, peers);
        const others = peers.filter((p) => p.peerId !== att.peerId);
        ws.send(
          JSON.stringify({ type: "joined", peerId: att.peerId, peers: others } satisfies ServerMessage),
        );
        await this.broadcast(
          { type: "peer-joined", peer: { peerId: att.peerId, userId: att.userId, username: msg.username } } satisfies ServerMessage,
          ws,
        );
        return;
      }

      case "offer":
      case "answer":
      case "ice-candidate": {
        if (!att.joined) {
          this.sendError(ws, "not_joined", "send join first");
          return;
        }
        const target = await this.resolvePeerSocket(msg.to);
        if (!target) {
          this.sendError(ws, "bad_message", `unknown peer: ${msg.to}`);
          return;
        }
        const relay: ServerMessage =
          msg.type === "offer"
            ? { type: "offer", from: att.peerId, sdp: msg.sdp }
            : msg.type === "answer"
              ? { type: "answer", from: att.peerId, sdp: msg.sdp }
              : { type: "ice-candidate", from: att.peerId, candidate: msg.candidate };
        target.send(JSON.stringify(relay));
        return;
      }

      case "presence": {
        if (!att.joined) {
          this.sendError(ws, "not_joined", "send join first");
          return;
        }
        await this.broadcast(
          { type: "presence", userId: att.userId, status: msg.status } satisfies ServerMessage,
          ws,
        );
        return;
      }

      case "ping": {
        ws.send(JSON.stringify({ type: "pong" } satisfies ServerMessage));
        return;
      }

      default: {
        this.sendError(ws, "bad_message", "unknown message type");
      }
    }
  }

  async webSocketClose(ws: WebSocket, _code: number, _reason: string, _wasClean: boolean): Promise<void> {
    await this.removePeer(ws);
  }

  async webSocketError(ws: WebSocket, _error: unknown): Promise<void> {
    await this.removePeer(ws);
  }

  /** Resolve a target peerId to its live socket via the persisted peer map. */
  private async resolvePeerSocket(toPeerId: string): Promise<WebSocket | null> {
    const peers = (await this.state.storage.get<PeerInfo[]>(PEERS_KEY)) ?? [];
    const target = peers.find((p) => p.peerId === toPeerId);
    if (!target) return null;
    const socket = this.findLiveSocket(target.peerId, target.userId);
    if (!socket) {
      // Ghost target — drop it from the map so relays don't keep failing.
      await this.state.storage.put(PEERS_KEY, peers.filter((p) => p.peerId !== toPeerId));
      return null;
    }
    return socket;
  }

  private findLiveSocket(peerId: string, userId: string): WebSocket | null {
    for (const socket of this.state.getWebSockets(userId)) {
      if (socket.readyState !== OPEN) continue;
      const att = socket.deserializeAttachment() as SocketAttachment | null;
      if (att?.peerId === peerId) return socket;
    }
    return null;
  }

  /** Remove the peer entry on close/error; broadcast peer-left if it had joined. */
  private async removePeer(ws: WebSocket): Promise<void> {
    const att = ws.deserializeAttachment() as SocketAttachment | null;
    if (!att) return;
    const peers = (await this.state.storage.get<PeerInfo[]>(PEERS_KEY)) ?? [];
    const idx = peers.findIndex((p) => p.peerId === att.peerId);
    if (idx === -1) return;
    peers.splice(idx, 1);
    await this.state.storage.put(PEERS_KEY, peers);
    if (att.joined) {
      const live = this.state.getWebSockets().filter((s) => s.readyState === OPEN).length;
      console.log(`[do] removePeer ${att.peerId.slice(0, 6)} joined=${att.joined} liveSockets=${live}`);
      await this.broadcast({ type: "peer-left", peerId: att.peerId } satisfies ServerMessage, ws);
    }
  }

  /**
   * Send to every joined peer. MUST resolve sockets from the peer map, not
   * `getWebSockets()`: inside a WebSocket event (message/close/error) the
   * Hibernation API scopes the tag-less call to the event socket's tags, so a
   * broadcast from `webSocketClose` would reach nobody else. Each peer's
   * socket is looked up explicitly by its own userId tag.
   */
  private async broadcast(message: ServerMessage, except?: WebSocket): Promise<void> {
    const payload = JSON.stringify(message);
    const peers = (await this.state.storage.get<PeerInfo[]>(PEERS_KEY)) ?? [];
    for (const p of peers) {
      const socket = this.findLiveSocket(p.peerId, p.userId);
      if (socket && socket !== except && socket.readyState === OPEN) {
        socket.send(payload);
      }
    }
  }

  private sendError(ws: WebSocket, code: string, message: string): void {
    if (ws.readyState !== OPEN) return;
    ws.send(JSON.stringify({ type: "error", code, message } satisfies ServerMessage));
  }
}
