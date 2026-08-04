import type { ClientMessage, ServerMessage } from "@lumen/protocol";

/**
 * WebSocket client for the channel Durable Object (docs/protocol.md §1).
 * Browser WebSocket cannot set the Authorization header, so the token rides
 * as ?token= on the upgrade URL — the worker validates it before delegating
 * to the DO.
 */
export class ChannelSignaling {
  private ws: WebSocket | null = null;
  private queue: ClientMessage[] = [];

  /** Set before connect(). */
  onMessage: ((msg: ServerMessage) => void) | null = null;
  onClose: ((code: number, reason: string) => void) | null = null;

  connect(baseUrl: string, token: string, channelId: string): Promise<void> {
    const wsBase = baseUrl.replace(/^http/, "ws");
    const url = `${wsBase}/api/ws/${encodeURIComponent(channelId)}?token=${encodeURIComponent(token)}`;
    const ws = new WebSocket(url);
    this.ws = ws;

    return new Promise((resolve, reject) => {
      let opened = false;
      ws.onopen = () => {
        opened = true;
        // Flush anything queued before the socket opened.
        while (this.queue.length > 0) ws.send(JSON.stringify(this.queue.shift()));
        resolve();
      };
      ws.onerror = () => {
        reject(new Error("signaling websocket error"));
      };
      ws.onmessage = (event) => {
        let msg: ServerMessage;
        try {
          msg = JSON.parse(String(event.data)) as ServerMessage;
        } catch {
          return;
        }
        this.onMessage?.(msg);
      };
      ws.onclose = (event) => {
        if (!opened) {
          // The server refused/closed the upgrade — surface it instead of
          // leaving join() hanging on a promise that never settles.
          reject(new Error(`signaling closed before open (${event.code}${event.reason ? `: ${event.reason}` : ""})`));
        }
        this.onClose?.(event.code, event.reason);
      };
    });
  }

  send(msg: ClientMessage): void {
    const ws = this.ws;
    if (ws && ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(msg));
    } else {
      this.queue.push(msg);
    }
  }

  close(): void {
    this.ws?.close(1000, "leave");
    this.ws = null;
  }
}
