import type { VoiceMesh } from "./mesh";
import { preferH264 } from "./screenShare";

/**
 * Screen-share transport — the swappable seam between the UI and the video
 * plumbing. The UI only talks to this interface, so a Cloudflare Realtime SFU
 * client can replace the mesh implementation later without touching the view.
 *
 * Current implementation: full-mesh (one encoder per viewer — the Fase 4
 * measured cost). Future: `createRealtimeSfuScreenTransport()` sharing one
 * encoder upstream.
 */

export interface ScreenShareTransport {
  /** Attach the captured stream to every peer, H.264 preferred. */
  start(stream: MediaStream): Promise<void>;
  /** Detach and stop sending. */
  stop(): Promise<void>;
}

export function createMeshScreenTransport(mesh: VoiceMesh): ScreenShareTransport {
  let currentTrack: MediaStreamTrack | null = null;

  return {
    async start(stream: MediaStream) {
      const track = stream.getVideoTracks()[0];
      if (!track) throw new Error("no video track in screen stream");
      currentTrack = track;
      await mesh.addVideoTrack(track, stream);
    },
    async stop() {
      if (currentTrack) {
        await mesh.removeVideoTrack(currentTrack);
        currentTrack = null;
      }
    },
  };
}

/** Apply the brief's codec/quality profile to a connection's video senders. */
export function configureScreenSenders(pc: RTCPeerConnection): void {
  preferH264(pc);
  for (const sender of pc.getSenders()) {
    const params = sender.getParameters();
    if (params.degradationPreference !== "maintain-resolution") {
      params.degradationPreference = "maintain-resolution";
      try {
        sender.setParameters(params);
      } catch {
        // not supported on this engine — degradationPreference is advisory
      }
    }
  }
}
