import type { Channel, PeerInfo } from "@lumen/protocol";
import { auth } from "./auth.svelte";
import { shell } from "./shell.svelte";
import { ChannelSignaling } from "$lib/media/signaling";
import { VoiceMesh } from "$lib/media/mesh";
import { createMicPipeline, type MicPipeline } from "$lib/media/audio";
import { LevelMeter } from "$lib/media/levels";
import { startScreenCapture, type ScreenCapture } from "$lib/media/screenShare";
import { createMeshScreenTransport, type ScreenShareTransport } from "$lib/media/screenTransport";

export interface VoicePeer {
  peerId: string;
  userId: string;
  username: string;
  stream: MediaStream | null;
  /** Shared screen from this peer, if any. */
  videoStream: MediaStream | null;
  level: number;
  speaking: boolean;
}

/** Speak threshold + hysteresis (levels are RMS 0..1). */
const SPEAK_ON = 0.03;
const SPEAK_OFF = 0.02;

function withHysteresis(level: number, wasSpeaking: boolean): boolean {
  return level > SPEAK_ON || (wasSpeaking && level > SPEAK_OFF);
}

/**
 * The active voice call: mic pipeline (RNNoise), full-mesh transport, remote
 * peer audio, speaking indicators, mute/deafen. One call at a time, per
 * channel. All WebRTC state lives here — the UI is a pure view over it.
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
  /** Screen share in progress: the captured stream, shown as a preview tile. */
  sharing = $state<MediaStream | null>(null);

  private mesh: VoiceMesh | null = null;
  private signaling: ChannelSignaling | null = null;
  private screenTransport: ScreenShareTransport | null = null;
  private screenCapture: ScreenCapture | null = null;
  private pipeline: MicPipeline | null = null;
  private localMeter: LevelMeter | null = null;
  private meters = new Map<string, LevelMeter>();
  private remoteCtx: AudioContext | null = null;
  private rafId = 0;
  private currentServerId: string | null = null;
  private usernameById = new Map<string, string>();
  private memberRefresh: Promise<void> | null = null;

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
      this.pipeline = await createMicPipeline();
      await this.pipeline.resume();
      this.localMeter = new LevelMeter(this.pipeline.analyser);

      this.signaling = new ChannelSignaling();
      // If the signaling socket drops, the call is over — clean up state.
      this.signaling.onClose = () => {
        if (this.channelId === channel.id) {
          this.error = "signaling disconnected";
          void this.leave();
        }
      };
      this.mesh = new VoiceMesh(this.signaling, config.iceServers as RTCIceServer[], {
        onPeerJoined: (peer) => void this.addPeer(peer),
        onRemoteStream: (peerId, stream) => this.attachRemoteStream(peerId, stream),
        onRemoteVideo: (peerId, stream) => this.attachRemoteVideo(peerId, stream),
        onPeerRemoved: (peerId) => this.removePeer(peerId),
        onError: (peerId, message) => {
          if (peerId === "") this.error = message;
        },
      });

      await this.signaling.connect(auth.backendUrl, auth.token!, channel.id);
      await this.mesh.join(channel.id, user.id, this.pipeline.stream);
      this.channelId = channel.id;
      this.connected = true;
      this.startLevelLoop();
    } catch (err) {
      this.error = err instanceof Error ? err.message : String(err);
      await this.leave();
    }
  }

  async leave(): Promise<void> {
    this.stopLevelLoop();
    await this.mesh?.leave();
    this.mesh = null;
    this.signaling = null;
    await this.stopShare();
    this.pipeline?.dispose();
    this.pipeline = null;
    void this.remoteCtx?.close();
    this.remoteCtx = null;
    this.localMeter = null;
    this.meters.clear();
    this.currentServerId = null;
    this.memberRefresh = null;
    this.channelId = null;
    this.connected = false;
    this.peers = [];
    this.localLevel = 0;
    this.localSpeaking = false;
  }

  async toggleShare(): Promise<void> {
    if (this.sharing) {
      await this.stopShare();
      return;
    }
    if (!this.mesh) return;
    try {
      const capture = await startScreenCapture();
      const transport = createMeshScreenTransport(this.mesh);
      await transport.start(capture.stream);
      this.screenTransport = transport;
      this.sharing = capture.stream;
      // Releasing the capture stops the local capture — keep a handle.
      this.screenCapture = capture;
    } catch (err) {
      this.error = err instanceof Error ? err.message : String(err);
    }
  }

  async stopShare(): Promise<void> {
    await this.screenTransport?.stop();
    this.screenTransport = null;
    this.screenCapture?.stop();
    this.screenCapture = null;
    this.sharing = null;
  }

  toggleMute(): void {
    this.muted = !this.muted;
    this.applyEnabled();
  }

  toggleDeafen(): void {
    this.deafened = !this.deafened;
    this.applyEnabled();
  }

  private applyEnabled(): void {
    this.pipeline?.setEnabled(!this.muted && !this.deafened);
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
      stream: null,
      videoStream: null,
      level: 0,
      speaking: false,
    });
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

  private attachRemoteStream(peerId: string, stream: MediaStream): void {
    const peer = this.peers.find((p) => p.peerId === peerId);
    if (!peer) return;
    peer.stream = stream;
    // Created in ontrack — outside any user gesture, so it starts suspended;
    // resume it or the analyser never receives audio.
    this.remoteCtx ??= new AudioContext();
    if (this.remoteCtx.state !== "running") void this.remoteCtx.resume();
    const source = this.remoteCtx.createMediaStreamSource(stream);
    const analyser = this.remoteCtx.createAnalyser();
    analyser.fftSize = 1024;
    source.connect(analyser);
    this.meters.set(peerId, new LevelMeter(analyser));
  }

  private attachRemoteVideo(peerId: string, stream: MediaStream): void {
    const peer = this.peers.find((p) => p.peerId === peerId);
    if (peer) peer.videoStream = stream;
  }

  private removePeer(peerId: string): void {
    this.meters.delete(peerId);
    const idx = this.peers.findIndex((p) => p.peerId === peerId);
    if (idx !== -1) this.peers.splice(idx, 1);
  }

  private startLevelLoop(): void {
    // rAF for scheduling (pauses when the tab is hidden) but only computes at
    // ~10 Hz — the level bars don't need 60 fps and each pass reads N analysers.
    let last = 0;
    const tick = (now: number) => {
      if (!this.connected) return;
      if (now - last >= 100) {
        last = now;
        const local = this.localMeter?.level() ?? 0;
        this.localLevel = local;
        this.localSpeaking = withHysteresis(local, this.localSpeaking);
        for (const peer of this.peers) {
          const meter = this.meters.get(peer.peerId);
          if (!meter) continue;
          const level = meter.level();
          peer.level = level;
          peer.speaking = withHysteresis(level, peer.speaking);
        }
      }
      this.rafId = requestAnimationFrame(tick);
    };
    this.rafId = requestAnimationFrame(tick);
  }

  private stopLevelLoop(): void {
    cancelAnimationFrame(this.rafId);
  }
}

export const voice = new VoiceStore();
