import type { Channel, PeerInfo } from "@lumen/protocol";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { auth } from "./auth.svelte";
import { shell } from "./shell.svelte";

export interface VoicePeer {
  peerId: string;
  userId: string;
  username: string;
  level: number;
  speaking: boolean;
  /** RTCPeerConnection state: new|connecting|connected|disconnected|failed|closed. */
  state: string;
}

/** Speak threshold + hysteresis (levels are RMS 0..1). */
const SPEAK_ON = 0.03;
const SPEAK_OFF = 0.02;

function withHysteresis(level: number, wasSpeaking: boolean): boolean {
  return level > SPEAK_ON || (wasSpeaking && level > SPEAK_OFF);
}

/** Read a string field from a Tauri IPC payload, validating it at the boundary. */
function readStr(payload: unknown, key: string): string | null {
  if (payload && typeof payload === "object" && key in payload) {
    const v = (payload as Record<string, unknown>)[key];
    return typeof v === "string" ? v : null;
  }
  return null;
}

/** Read a number field from a Tauri IPC payload, validating it at the boundary. */
function readNum(payload: unknown, key: string): number | null {
  if (payload && typeof payload === "object" && key in payload) {
    const v = (payload as Record<string, unknown>)[key];
    return typeof v === "number" ? v : null;
  }
  return null;
}

/**
 * The active voice call. All media lives in the native Rust client (cpal mic,
 * opus, webrtc-rs); this store is a thin control/view layer over the Tauri
 * commands and `voice://` events. One call at a time, per channel.
 */
class VoiceStore {
  channelId = $state<string | null>(null);
  connected = $state(false);
  muted = $state(false);
  deafened = $state(false);
  peers = $state<VoicePeer[]>([]);
  localLevel = $state(0);
  localSpeaking = $state(false);
  error = $state<string | null>(null);

  /** Ring buffer of signaling/negotiation events, shown in the UI debug panel. */
  log = $state<{ t: string; msg: string }[]>([]);

  private unlisteners: UnlistenFn[] = [];
  private usernameById = new Map<string, string>();
  private currentServerId: string | null = null;
  private memberRefresh: Promise<void> | null = null;

  private pushLog(msg: string): void {
    this.log = [...this.log.slice(-29), { t: new Date().toLocaleTimeString("es-ES"), msg }];
  }

  async join(channel: Channel): Promise<void> {
    if (this.channelId === channel.id && this.connected) return;
    await this.leave();

    const user = auth.user;
    if (!user) return;
    this.error = null;
    try {
      this.currentServerId = channel.serverId;
      if (channel.kind === "dm") {
        const dm = shell.dmList.find((d) => d.channel.id === channel.id);
        const other = shell.friends.find((f) => f.user.username === dm?.otherUsername);
        this.usernameById = new Map(other ? [[other.user.id, other.user.username]] : []);
      } else {
        const { members } = await auth.api.getServer(channel.serverId);
        this.usernameById = new Map(members.map((m) => [m.id, m.username]));
      }

      const config = await auth.api.getRealtimeConfig();
      this.pushLog(`config: ${config.iceServers.map((s) => s.urls).join(",").slice(0, 120)}`);
      await this.subscribe();

      this.pushLog("invoke voice_join");
      await invoke("voice_join", {
        args: {
          backendUrl: auth.backendUrl,
          token: auth.token!,
          channelId: channel.id,
          userId: user.id,
          iceServers: config.iceServers,
        },
      });
      this.pushLog("joined channel");
      this.channelId = channel.id;
      this.connected = true;
    } catch (err) {
      this.error = err instanceof Error ? err.message : String(err);
      this.pushLog(`JOIN FAILED: ${err instanceof Error ? err.message : String(err)}`);
      await this.leave();
    }
  }

  async leave(): Promise<void> {
    this.pushLog("leave");
    try {
      await invoke("voice_leave");
    } catch {
      // nothing to leave — fine
    }
    for (const unlisten of this.unlisteners) unlisten();
    this.unlisteners = [];
    this.currentServerId = null;
    this.memberRefresh = null;
    this.channelId = null;
    this.connected = false;
    this.peers = [];
    this.localLevel = 0;
    this.localSpeaking = false;
  }

  toggleMute(): void {
    this.muted = !this.muted;
    void invoke("voice_set_muted", { muted: this.muted });
  }

  toggleDeafen(): void {
    this.deafened = !this.deafened;
    void invoke("voice_set_deafened", { deafened: this.deafened });
  }

  /** Wire the `voice://` event stream to store state (idempotent). */
  private async subscribe(): Promise<void> {
    if (this.unlisteners.length > 0) return;
    this.unlisteners = [
      await listen("voice://peer-joined", (e) => {
        const peerId = readStr(e.payload, "peerId");
        const userId = readStr(e.payload, "userId");
        if (!peerId || !userId) return;
        this.pushLog(`peer-joined ${userId.slice(0, 8)}`);
        void this.addPeer({ peerId, userId } as PeerInfo);
      }),
      await listen("voice://peer-left", (e) => {
        const peerId = readStr(e.payload, "peerId");
        if (!peerId) return;
        this.pushLog(`peer-left ${peerId.slice(0, 8)}`);
        this.removePeer(peerId);
      }),
      await listen("voice://state", (e) => {
        const peerId = readStr(e.payload, "peerId");
        const state = readStr(e.payload, "state");
        if (!peerId || !state) return;
        this.pushLog(`state ${peerId.slice(0, 8)} -> ${state}`);
        const peer = this.peers.find((p) => p.peerId === peerId);
        if (peer) peer.state = state;
      }),
      await listen("voice://levels", (e) => {
        const local = readNum(e.payload, "local");
        if (local === null) return;
        const rawPeers = e.payload && typeof e.payload === "object" && "peers" in e.payload
          ? (e.payload as Record<string, unknown>).peers
          : null;
        const peers = Array.isArray(rawPeers) ? rawPeers : [];
        this.localLevel = local;
        this.localSpeaking = withHysteresis(local, this.localSpeaking);
        for (const p of peers) {
          const peerId = readStr(p, "peerId");
          const level = readNum(p, "level");
          if (!peerId || level === null) continue;
          const peer = this.peers.find((x) => x.peerId === peerId);
          if (!peer) continue;
          peer.level = level;
          peer.speaking = withHysteresis(level, peer.speaking);
        }
      }),
      await listen("voice://error", (e) => {
        const code = readStr(e.payload, "code");
        const message = readStr(e.payload, "message") ?? String(e.payload);
        this.pushLog(`ERROR: ${code ? `${code} ` : ""}${message}`);
        this.error = message;
      }),
      await listen("voice://debug", (e) => {
        const msg = readStr(e.payload, "message") ?? String(e.payload);
        this.pushLog(msg);
      }),
      await listen("voice://signaling", (e) => {
        const state = readStr(e.payload, "state") ?? "";
        this.pushLog(`signaling ${state}`);
      }),
    ];
  }

  private async addPeer(peer: PeerInfo): Promise<void> {
    if (this.peers.some((p) => p.peerId === peer.peerId)) return;
    const username = await this.resolveUsername(peer.userId);
    // A leave may have raced the username fetch — don't re-add a gone peer.
    if (this.peers.some((p) => p.peerId === peer.peerId)) return;
    this.peers.push({
      peerId: peer.peerId,
      userId: peer.userId,
      username,
      level: 0,
      speaking: false,
      state: "new",
    });
  }

  private removePeer(peerId: string): void {
    const idx = this.peers.findIndex((p) => p.peerId === peerId);
    if (idx !== -1) this.peers.splice(idx, 1);
  }

  /** Members snapshot may predate a peer joining the guild — refetch once. */
  private async resolveUsername(userId: string): Promise<string> {
    const cached = this.usernameById.get(userId);
    if (cached) return cached;
    this.memberRefresh ??= this.refreshMembers();
    await this.memberRefresh;
    return this.usernameById.get(userId) ?? userId.slice(0, 8);
  }

  private async refreshMembers(): Promise<void> {
    if (!this.currentServerId) return;
    try {
      const { members } = await auth.api.getServer(this.currentServerId);
      this.usernameById = new Map(members.map((m) => [m.id, m.username]));
    } catch {
      // keep whatever we had
    }
  }
}

export const voice = new VoiceStore();
