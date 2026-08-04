import type { PeerInfo, ServerMessage } from "@lumen/protocol";
import { preferH264 } from "./screenShare";
import type { ChannelSignaling } from "./signaling";

/**
 * Full-mesh voice transport: one RTCPeerConnection per peer (≤3 others), all
 * media P2P, signaling relayed through the channel DO.
 *
 * Negotiation model (glare-free): the peer that joins later waits; each
 * already-present peer initiates an offer to the newcomer. ICE is trickled.
 * No renegotiation in v1 — the local stream is static after join (mute is
 * `track.enabled = false`, which sends silence without renegotiation).
 */

export interface MeshEvents {
  onPeerJoined(peer: PeerInfo): void;
  onRemoteStream(peerId: string, stream: MediaStream): void;
  onRemoteVideo(peerId: string, stream: MediaStream): void;
  onPeerRemoved(peerId: string): void;
  onError(peerId: string, message: string): void;
  /** ICE/connection state of a peer's RTCPeerConnection. */
  onState(peerId: string, state: string): void;
  /** Signaling/negotiation trace for the in-app debug log. */
  onDebug(msg: string): void;
}

interface PeerConnectionEntry {
  peerId: string;
  userId: string;
  pc: RTCPeerConnection;
}

export class VoiceMesh {
  private peers = new Map<string, PeerConnectionEntry>();
  private localStream: MediaStream | null = null;

  constructor(
    private readonly signaling: ChannelSignaling,
    private readonly iceServers: RTCIceServer[],
    private readonly events: MeshEvents,
  ) {
    this.signaling.onMessage = (msg) => this.handleMessage(msg);
  }

  /** Connect to the channel DO and send join. Media starts flowing after
   *  negotiation completes on each side. */
  async join(channelId: string, userId: string, stream: MediaStream): Promise<void> {
    this.localStream = stream;
    this.signaling.send({ type: "join", channelId, userId });
  }

  async leave(): Promise<void> {
    for (const entry of this.peers.values()) entry.pc.close();
    this.peers.clear();
    this.localStream = null;
    this.signaling.close();
  }

  /**
   * Add a video track (screen share) to every peer connection and renegotiate.
   * Only the sharer initiates, so there is no glare; viewers answer through
   * the normal `offer` path. Idempotent per track.
   */
  async addVideoTrack(track: MediaStreamTrack, stream: MediaStream): Promise<void> {
    const added: Promise<void>[] = [];
    for (const entry of this.peers.values()) {
      if (entry.pc.getSenders().some((s) => s.track === track)) continue;
      entry.pc.addTrack(track, stream);
      added.push(this.sendOffer(entry).catch(() => this.events.onError(entry.peerId, "renegotiation failed")));
    }
    await Promise.all(added);
  }

  /** Remove a shared video track from every peer connection and renegotiate. */
  async removeVideoTrack(track: MediaStreamTrack): Promise<void> {
    const done: Promise<void>[] = [];
    for (const entry of this.peers.values()) {
      const sender = entry.pc.getSenders().find((s) => s.track === track);
      if (!sender) continue;
      entry.pc.removeTrack(sender);
      done.push(this.sendOffer(entry).catch(() => this.events.onError(entry.peerId, "renegotiation failed")));
    }
    await Promise.all(done);
  }

  private handleMessage(msg: ServerMessage): void {
    switch (msg.type) {
      case "joined": {
        // We joined after these peers → they will offer us; just create the PCs.
        this.events.onDebug(`joined: ${msg.peers.length} existing peer(s)`);
        for (const peer of msg.peers) this.setupPeer(peer);
        break;
      }
      case "peer-joined": {
        // They joined after us → we initiate.
        this.events.onDebug(`peer-joined ${msg.peer.userId.slice(0, 8)}`);
        const entry = this.setupPeer(msg.peer);
        if (!entry) break;
        void this.sendOffer(entry).catch(() => this.events.onError(entry.peerId, "offer failed"));
        break;
      }
      case "offer": {
        const entry = this.peers.get(msg.from);
        this.events.onDebug(`offer from ${msg.from.slice(0, 8)} (pc: ${entry ? "yes" : "NO"})`);
        if (!entry) {
          this.events.onError(msg.from, "offer from unknown peer");
          return;
        }
        void this.acceptOffer(entry, msg.sdp).catch(() =>
          this.events.onError(entry.peerId, "answer failed"),
        );
        break;
      }
      case "answer": {
        const entry = this.peers.get(msg.from);
        this.events.onDebug(`answer from ${msg.from.slice(0, 8)} (pc: ${entry ? "yes" : "NO"})`);
        if (!entry || !entry.pc.remoteDescription) return;
        void entry.pc
          .setRemoteDescription({ type: "answer", sdp: msg.sdp })
          .catch(() => this.events.onError(entry.peerId, "setRemoteDescription failed"));
        break;
      }
      case "ice-candidate": {
        const entry = this.peers.get(msg.from);
        if (!entry) return;
        this.events.onDebug(`ice-candidate from ${msg.from.slice(0, 8)}`);
        void entry.pc
          .addIceCandidate(msg.candidate as RTCIceCandidateInit)
          .catch(() => this.events.onError(entry.peerId, "bad ice candidate"));
        break;
      }
      case "peer-left": {
        this.removePeer(msg.peerId);
        break;
      }
      default:
        break;
    }
  }

  /**
   * Create the peer connection, surfacing the peer in the UI even if the
   * RTCPeerConnection can't be built (e.g. WebRTC unavailable in the WebView).
   * Returns null when the connection could not be created.
   */
  private setupPeer(peer: PeerInfo): PeerConnectionEntry | null {
    try {
      return this.createPeer(peer);
    } catch (err) {
      this.events.onPeerJoined(peer);
      this.events.onError(
        peer.peerId,
        `peer setup failed: ${err instanceof Error ? err.message : String(err)}`,
      );
      return null;
    }
  }

  private createPeer(peer: PeerInfo): PeerConnectionEntry {
    const pc = new RTCPeerConnection({ iceServers: this.iceServers });
    // H.264 first in the codec list from the start, so offers/answers prefer it.
    preferH264(pc);
    const entry: PeerConnectionEntry = { peerId: peer.peerId, userId: peer.userId, pc };
    this.peers.set(peer.peerId, entry);
    this.events.onPeerJoined(peer);

    for (const track of this.localStream?.getTracks() ?? []) {
      pc.addTrack(track, this.localStream!);
    }
    pc.onicecandidate = (event) => {
      if (event.candidate) {
        this.signaling.send({
          type: "ice-candidate",
          to: peer.peerId,
          candidate: event.candidate.toJSON(),
        });
      }
    };
    pc.ontrack = (event) => {
      const stream = event.streams[0];
      if (!stream) return;
      if (event.track.kind === "video") {
        this.events.onRemoteVideo(peer.peerId, stream);
      } else {
        this.events.onRemoteStream(peer.peerId, stream);
      }
    };
    pc.onconnectionstatechange = () => {
      this.events.onState(peer.peerId, pc.connectionState);
      if (pc.connectionState === "failed" || pc.connectionState === "closed") {
        this.events.onError(peer.peerId, `connection ${pc.connectionState}`);
      }
    };
    return entry;
  }

  private async sendOffer(entry: PeerConnectionEntry): Promise<void> {
    const offer = await entry.pc.createOffer();
    await entry.pc.setLocalDescription(offer);
    this.signaling.send({ type: "offer", to: entry.peerId, sdp: offer.sdp! });
  }

  private async acceptOffer(entry: PeerConnectionEntry, sdp: string): Promise<void> {
    await entry.pc.setRemoteDescription({ type: "offer", sdp });
    const answer = await entry.pc.createAnswer();
    await entry.pc.setLocalDescription(answer);
    this.signaling.send({ type: "answer", to: entry.peerId, sdp: answer.sdp! });
  }

  private removePeer(peerId: string): void {
    const entry = this.peers.get(peerId);
    if (!entry) return;
    entry.pc.close();
    this.peers.delete(peerId);
    this.events.onPeerRemoved(peerId);
  }
}
