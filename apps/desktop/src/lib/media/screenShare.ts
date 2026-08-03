/**
 * Screen capture with the efficiency profile from the project brief:
 *  - framerate capped at 15 fps;
 *  - `track.contentHint = "detail"` (static content, e.g. code);
 *  - H.264 preferred (hardware encode when the platform exposes it) via
 *    `setCodecPreferences` on every video sender (applied by screenTransport);
 *  - `degradationPreference: "maintain-resolution"` on the sender (applied by
 *    screenTransport — the encoder drops frames rather than shrinking).
 */

export interface ScreenCapture {
  stream: MediaStream;
  stop(): void;
}

const CAPTURE_CONSTRAINTS: MediaStreamConstraints = {
  video: { frameRate: { ideal: 15, max: 15 } } as MediaTrackConstraints,
  audio: false,
};

export async function startScreenCapture(): Promise<ScreenCapture> {
  const stream = await navigator.mediaDevices.getDisplayMedia(CAPTURE_CONSTRAINTS);
  for (const track of stream.getVideoTracks()) {
    track.contentHint = "detail";
  }
  return {
    stream,
    stop() {
      for (const track of stream.getTracks()) track.stop();
    },
  };
}

/**
 * Reorder a connection's video transceivers so H.264 is preferred over VP8/VP9
 * (H.264 is what a hardware encoder can accelerate). No-op on a WebView that
 * doesn't expose H.264 encoding — reported, not forced.
 */
export function preferH264(pc: RTCPeerConnection): void {
  const caps = RTCRtpSender.getCapabilities("video");
  if (!caps) return;
  const h264 = caps.codecs.filter((c) => c.mimeType.toLowerCase().includes("h264"));
  if (h264.length === 0) return;
  for (const transceiver of pc.getTransceivers()) {
    if (transceiver.sender?.track?.kind === "video") {
      try {
        transceiver.setCodecPreferences(h264);
      } catch {
        // setCodecPreferences after negotiation — default order stands
      }
    }
  }
}

/** H.264/VP* codecs this WebView can encode, for the per-platform report. */
export function videoEncodeCapabilities(): string[] {
  if (typeof RTCRtpSender === "undefined" || !RTCRtpSender.getCapabilities) return [];
  const caps = RTCRtpSender.getCapabilities("video");
  return caps ? caps.codecs.map((c) => c.mimeType) : [];
}