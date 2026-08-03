import type { PeerInfo, ServerMessage } from "@lumen/protocol";
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
  onPeerRemoved(peerId: string): void;
  onError(peerId: string, message: string): void;
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

  private handleMessage(msg: ServerMessage): void {
    switch (msg.type) {
      case "joined": {
        // We joined after these peers → they will offer us; just create the PCs.
        for (const peer of msg.peers) this.createPeer(peer);
        break;
      }
      case "peer-joined": {
        // They joined after us → we initiate.
        const entry = this.createPeer(msg.peer);
        void this.sendOffer(entry).catch(() => this.events.onError(entry.peerId, "offer failed"));
        break;
      }
      case "offer": {
        const entry = this.peers.get(msg.from);
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
        if (!entry || !entry.pc.remoteDescription) return;
        void entry.pc
          .setRemoteDescription({ type: "answer", sdp: msg.sdp })
          .catch(() => this.events.onError(entry.peerId, "setRemoteDescription failed"));
        break;
      }
      case "ice-candidate": {
        const entry = this.peers.get(msg.from);
        if (!entry) return;
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

  private createPeer(peer: PeerInfo): PeerConnectionEntry {
    const pc = new RTCPeerConnection({ iceServers: this.iceServers });
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
      if (stream) this.events.onRemoteStream(peer.peerId, stream);
    };
    pc.onconnectionstatechange = () => {
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
