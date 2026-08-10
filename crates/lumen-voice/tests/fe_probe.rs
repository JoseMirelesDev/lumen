//! FastEnhancer (faster-enhancer.c, 48 kHz int8 runtime) smoke test.
//! Feeds synthetic noise + a speech-like tone through the denoiser and checks
//! the two contracts that matter for the send path:
//!   - stationary noise is suppressed hard (the mic never opens on noise), and
//!   - a speech-like tone survives (no "suprime la voz").
//!
//! Skipped (returns early) on CPUs without AVX2+FMA3+F16C where `new()` is
//! None — that path is the NS-only auto-fallback.

use lumen_voice::audio::rms_level;

#[test]
fn fastenhancer_suppresses_noise_preserves_tone() {
    let Some(mut fe) = lumen_voice::audio::FastEnhancerDenoiser::new() else {
        eprintln!("fe unavailable (no AVX2+FMA3+F16C) — skipping");
        return;
    };

    const RATE: usize = 48_000;
    const FRAME: usize = 960; // 20 ms
    let secs = 8;
    let n = RATE * secs;
    let mut state = 0x1234_5678u32;
    let mut input: Vec<i16> = Vec::with_capacity(n);
    for i in 0..n {
        // xorshift white noise, then a 220 Hz "speech" tone in the 2nd half.
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let noise = ((state >> 8) as i16) as f32 / 4.0 / 32768.0;
        let t = i as f32 / RATE as f32;
        let tone = if i >= RATE * secs / 2 {
            (2.0 * std::f32::consts::PI * 220.0 * t).sin() * 0.25
        } else {
            0.0
        };
        input.push(((noise * 32767.0 + tone * 32767.0).round().clamp(-32768.0, 32767.0)) as i16);
    }

    let mut out: Vec<i16> = Vec::with_capacity(n);
    for frame in input.chunks_exact(FRAME) {
        out.extend(fe.process(frame));
    }

    let rms = |a: usize, b: usize| rms_level(&out[a..b]);
    // Skip the 704-sample (14.67 ms) STFT alignment delay: ignore ~20 frames.
    let f = 20 * FRAME;
    // 1st half = noise only; 2nd half = noise + tone. Tone sits in [3s, 8s].
    let noise_out = rms(f, 2 * RATE);
    let tone_out = rms(4 * RATE, n);

    let noise_in = rms_level(&input[0..2 * RATE]);
    let tone_in = rms_level(&input[4 * RATE..n]);

    println!(
        "noise: in {:.3} -> out {:.5} ({:.1} dB) | tone: in {:.3} -> out {:.3}",
        noise_in,
        noise_out,
        20.0 * (noise_out / noise_in.max(1e-9)).log10(),
        tone_in,
        tone_out
    );

    // Noise floor crushed well below the input (Krisp-like gate).
    assert!(noise_out < noise_in * 0.03, "noise not suppressed: {noise_out} vs {noise_in}");
    // Speech-like tone survives (not cut to silence).
    assert!(tone_out > tone_in * 0.3, "tone attenuated too far: {tone_out} vs {tone_in}");
}
