#!/usr/bin/env python3
"""Vox UI sound kit generator.

Synthesizes the minimalist UI sound set for the Lumen Slint client
(design: quiet, short, non-gamey, "medieval wood + dry click" character).

Output: 48 kHz mono PCM i16 WAVs in apps/lumen-slint/sounds/.
Every sound is:
  - short (< 200 ms)
  - quiet (peak ~= -18 dBFS) — a UI layer, never a game jingle
  - one gesture, no melodic loops, no attack transients above ~ -12 dBFS

Regenerate with:  python3 scripts/gen-ui-sounds.py
"""
import math
import struct
import wave
from pathlib import Path

SR = 48000
OUT = Path(__file__).resolve().parent.parent / "apps" / "lumen-slint" / "sounds"


def env_exp(n, tau_s):
    """Exponential decay envelope, 1.0 at sample 0."""
    return [math.exp(-i / SR / tau_s) for i in range(n)]


def sine(freq, n, phase=0.0):
    return [math.sin(2 * math.pi * freq * i / SR + phase) for i in range(n)]


def make_sound(samples, peak_db=-18.0):
    """Normalize to peak_db, clamp, return i16 bytes."""
    peak = max(1e-9, max(abs(s) for s in samples))
    gain = 10 ** (peak_db / 20.0) / peak
    out = bytearray()
    for s in samples:
        v = max(-1.0, min(1.0, s * gain))
        out += struct.pack("<h", int(v * 32767))
    return bytes(out)


def write(name, samples, peak_db=-18.0):
    data = make_sound(samples, peak_db)
    OUT.mkdir(parents=True, exist_ok=True)
    with wave.open(str(OUT / f"{name}.wav"), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(data)
    print(f"  {name}.wav  {len(samples)/SR*1000:5.1f} ms  {len(data)//1024} KB")


def additive(freqs, n, tau_s, harmonics=(1.0,)):
    """Sum of detuned sine partials with shared exp decay (soft body)."""
    body = [0.0] * n
    for fi, (f, amp) in enumerate(zip(freqs, harmonics)):
        # tiny detune between left/right partials gives a warm, non-electronic body
        det = f * (1.0 + 0.0012 * fi)
        for i, s in enumerate(sine(det, n, phase=0.5 * fi)):
            body[i] += s * amp
    e = env_exp(n, tau_s)
    return [body[i] * e[i] for i in range(n)]


def noise_burst(n, tau_s, lp=0.25):
    """Exponentially decaying filtered-ish noise (dry click)."""
    import random

    rng = random.Random(42)
    prev = 0.0
    out = []
    for i in range(n):
        raw = rng.uniform(-1.0, 1.0)
        prev = prev + lp * (raw - prev)  # one-pole lowpass
        out.append(prev)
    e = env_exp(n, tau_s)
    return [out[i] * e[i] for i in range(n)]


def main():
    print("Vox UI sound kit ->", OUT)

    # --- join: low wood-thunk (channel enter) ---
    n = int(0.14 * SR)
    write("join", additive([168, 252], n, 0.030, harmonics=(1.0, 0.35)), peak_db=-16)

    # --- leave: same family, darker + shorter ---
    n = int(0.10 * SR)
    write("leave", additive([140, 210], n, 0.024, harmonics=(1.0, 0.3)), peak_db=-18)

    # --- mute on: dry click, short, dull ---
    n = int(0.045 * SR)
    click = noise_burst(n, 0.008)
    body = additive([240], n, 0.012, harmonics=(1.0,))
    write("mute-on", [a * 0.8 + b * 0.5 for a, b in zip(click, body)], peak_db=-20)

    # --- mute off: click + tiny bright harmonic ---
    n = int(0.040 * SR)
    click = noise_burst(n, 0.007)
    body = additive([520, 780], n, 0.010, harmonics=(1.0, 0.25))
    write("mute-off", [a * 0.7 + b * 0.6 for a, b in zip(click, body)], peak_db=-20)

    # --- deafen on: low double-thud ---
    n = int(0.16 * SR)
    thud1 = additive([104, 156], int(0.07 * SR), 0.018, harmonics=(1.0, 0.4))
    thud2 = additive([92, 138], int(0.09 * SR), 0.020, harmonics=(1.0, 0.4))
    write("deafen-on", thud1 + thud2, peak_db=-19)

    # --- deafen off: single light thud ---
    n = int(0.08 * SR)
    write("deafen-off", additive([120, 180], n, 0.018, harmonics=(1.0, 0.35)), peak_db=-21)

    # --- message sent: near-silent tick ---
    n = int(0.028 * SR)
    write("send", additive([1100, 1650], n, 0.006, harmonics=(1.0, 0.2)), peak_db=-26)

    # --- message received: warm two-part tick ---
    n = int(0.05 * SR)
    t1 = additive([660, 990], int(0.02 * SR), 0.006, harmonics=(1.0, 0.3))
    t2 = additive([880, 1320], int(0.03 * SR), 0.007, harmonics=(1.0, 0.25))
    write("receive", t1 + t2, peak_db=-22)

    # --- user joined: two quiet wood notes ---
    n1 = int(0.07 * SR)
    n2 = int(0.09 * SR)
    note1 = additive([196, 294], n1, 0.022, harmonics=(1.0, 0.3))
    note2 = additive([262, 393], n2, 0.024, harmonics=(1.0, 0.3))
    write("user-join", note1 + note2, peak_db=-22)

    # --- user left: one descending low note ---
    n = int(0.11 * SR)
    freq_sweep = []
    for i in range(n):
        t = i / n
        f = 190.0 - 50.0 * t  # gentle downward
        freq_sweep.append(math.sin(2 * math.pi * f * i / SR))
    e = env_exp(n, 0.026)
    write("user-left", [freq_sweep[i] * e[i] for i in range(n)], peak_db=-22)

    # --- error: muted low buzz, no alarm siren ---
    n = int(0.10 * SR)
    buzz = [math.sin(2 * math.pi * 96 * i / SR) * (0.6 + 0.4 * math.sin(2 * math.pi * 7 * i / SR)) for i in range(n)]
    e = env_exp(n, 0.022)
    write("error", [buzz[i] * e[i] for i in range(n)], peak_db=-16)

    # --- dm call start: two soft bell-ish notes, very quiet ---
    n1 = int(0.09 * SR)
    n2 = int(0.13 * SR)
    b1 = additive([392, 588], n1, 0.020, harmonics=(1.0, 0.22))
    b2 = additive([523, 785], n2, 0.024, harmonics=(1.0, 0.22))
    write("call", b1 + b2, peak_db=-24)

    print("done.")


if __name__ == "__main__":
    main()
