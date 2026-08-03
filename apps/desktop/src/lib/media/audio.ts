import { RnnoiseWorkletNode, loadRnnoise } from "@sapphi-red/web-noise-suppressor";
import rnnoiseWorkletUrl from "@sapphi-red/web-noise-suppressor/rnnoiseWorklet.js?url";
import rnnoiseSimdUrl from "@sapphi-red/web-noise-suppressor/rnnoise_simd.wasm?url";
import rnnoiseUrl from "@sapphi-red/web-noise-suppressor/rnnoise.wasm?url";

/**
 * Microphone pipeline:
 *
 *   getUserMedia (echoCancellation + autoGainControl + noiseSuppression native)
 *     → RNNoise AudioWorklet (WASM, 48 kHz mono, @sapphi-red/web-noise-suppressor)
 *     → MediaStreamAudioDestination (send this to peers)
 *     → AnalyserNode (local speaking level)
 *
 * ADR 0001: RNNoise library choice and rationale live in docs/decisions/.
 */

export interface MicPipeline {
  /** Denoised output stream — attach to every RTCPeerConnection. */
  stream: MediaStream;
  /** Analyser over the processed output, for the local speaking indicator. */
  analyser: AnalyserNode;
  resume(): Promise<void>;
  setEnabled(enabled: boolean): void;
  dispose(): void;
}

const GUM_CONSTRAINTS: MediaTrackConstraints = {
  echoCancellation: true,
  autoGainControl: true,
  noiseSuppression: true,
  channelCount: 1,
  sampleRate: 48000,
};

export async function createMicPipeline(): Promise<MicPipeline> {
  const raw = await navigator.mediaDevices.getUserMedia({ audio: GUM_CONSTRAINTS });
  const ctx = new AudioContext({ sampleRate: 48000 });

  const wasmBinary = await loadRnnoise({ url: rnnoiseUrl, simdUrl: rnnoiseSimdUrl });
  await ctx.audioWorklet.addModule(rnnoiseWorkletUrl);
  const denoiser = new RnnoiseWorkletNode(ctx, { maxChannels: 1, wasmBinary });

  const source = ctx.createMediaStreamSource(raw);
  const out = ctx.createMediaStreamDestination();
  const analyser = ctx.createAnalyser();
  analyser.fftSize = 1024;

  source.connect(denoiser);
  denoiser.connect(out);
  denoiser.connect(analyser);

  const tracks = raw.getAudioTracks();

  return {
    stream: out.stream,
    analyser,
    resume: () => ctx.resume(),
    setEnabled(enabled: boolean) {
      for (const track of tracks) track.enabled = enabled;
    },
    dispose() {
      for (const track of tracks) track.stop();
      void ctx.close();
    },
  };
}
