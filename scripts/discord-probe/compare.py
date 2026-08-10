#!/usr/bin/env python3
"""Compare a processed (Krisp / FastEnhancer / Lumen) output against the noisy
input on the same metrics used by the lumen-voice harness:
  floor        : 10th-pct RMS of 100 ms windows (dBFS)  — noise suppression
  voice p90    : 90th-pct RMS of 100 ms windows (dBFS)  — speech level
  gain CV      : std/mean of per-frame out/in gain on voice frames — volume
                 oscillation (the thing the user hears as pumping)
  deep dips    : % of voice frames cut below 0.1x
Usage:
  compare.py input.wav output.wav [label]
"""
import sys, wave
import numpy as np

def load(p):
    with wave.open(p, "rb") as w:
        n = w.getnframes()
        d = np.frombuffer(w.readframes(n), dtype="<i2").astype(np.float64)
        if w.getnchannels() > 1:
            d = d.reshape(-1, w.getnchannels()).mean(axis=1)
        return d

def rms(x): return float(np.sqrt(np.mean(x**2)))

def frames_rms(x, hop=960):
    n = len(x) // hop
    return np.array([rms(x[i*hop:(i+1)*hop]) for i in range(n)])

def analyze(out, inp):
    fin = frames_rms(inp)
    fo = frames_rms(out)
    n = min(len(fin), len(fo))
    fin, fo = fin[:n], fo[:n]
    # noise floor / voice via 100 ms windows
    ow = sorted(rms(out[i:i+4800]) for i in range(0, len(out)-4800+1, 4800))
    floor = ow[len(ow)//10]; voice = ow[int(len(ow)*0.9)]
    # gain CV on voice frames
    m = fin > 0.02
    g = fo[m] / np.maximum(fin[m], 1e-9)
    cv = float(np.std(g)/np.mean(g))
    deep = float(np.mean(g < 0.1)*100)
    return (20*np.log10(max(floor,1e-9)/32768),
            20*np.log10(max(voice,1e-9)/32768),
            cv, deep)

def main():
    inp, out, label = load(sys.argv[1]), load(sys.argv[2]), sys.argv[3] if len(sys.argv) > 3 else "out"
    fl, vo, cv, deep = analyze(out, inp)
    in_fl, in_vo, _, _ = analyze(inp, inp)
    print(f"{label:24s} floor {fl:6.1f} dBFS (input {in_fl:6.1f}, -{(in_fl-fl):.1f}dB) "
          f"| voice {vo:6.1f} dBFS (input {in_vo:6.1f}) | gainCV {cv:.3f} | deep-dips {deep:.1f}%")

if __name__ == "__main__":
    main()
