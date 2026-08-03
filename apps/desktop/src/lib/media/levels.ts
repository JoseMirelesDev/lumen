/**
 * Speaking-level measurement: RMS of the time-domain window from an
 * AnalyserNode, mapped to 0..1. Used for both the local mic (processed
 * output) and each remote stream. Threshold + hysteresis live in the store.
 */
export class LevelMeter {
  private readonly data: Uint8Array;

  constructor(private readonly analyser: AnalyserNode) {
    this.data = new Uint8Array(analyser.fftSize);
  }

  /** RMS amplitude of the current window, 0..1. */
  level(): number {
    this.analyser.getByteTimeDomainData(this.data);
    let sum = 0;
    for (let i = 0; i < this.data.length; i++) {
      const d = (this.data[i]! - 128) / 128;
      sum += d * d;
    }
    return Math.sqrt(sum / this.data.length);
  }
}
