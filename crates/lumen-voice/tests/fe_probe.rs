//! FastEnhancer (faster-enhancer.c, 48 kHz int8 runtime) smoke tests.
//! Feeds synthetic noise + a speech-like tone through the denoiser and checks
//! the two contracts that matter for the send path:
//!   - stationary noise is suppressed hard (the mic never opens on noise), and
//!   - a speech-like tone survives (no "suprime la voz").
//! Both tiers are exercised: Medium ("Ultra", hop 320) and Small ("Ligera",
//! hop 512). The Small wrapper re-frames the app's 960-sample frames onto the
//! engine's 512-sample grid, so its output length must track the input.
//!
//! Skipped (returns early) on CPUs without AVX2+FMA3+F16C where `new()` is
//! None — that path is the NS-only auto-fallback.

use lumen_voice::audio::{rms_level, FastEnhancerDenoiser};

const RATE: usize = 48_000;
const FRAME: usize = 960; // 20 ms

fn run_tier_contract(label: &str, fe: &mut FastEnhancerDenoiser) {
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

    // The wrapper returns complete input-length blocks; total output may be a
    // partial frame short of the input (the tail stays buffered).
    assert!(
        out.len() >= n - FRAME,
        "{label}: output too short: {} vs input {n}",
        out.len()
    );

    let rms = |a: usize, b: usize| rms_level(&out[a..b]);
    // Skip the STFT alignment delay (M: 704 samples; S: 512) + wrapper
    // re-framing: ignore the first ~20 frames.
    let f = 20 * FRAME;
    // 1st half = noise only; 2nd half = noise + tone. Tone sits in [3s, 8s].
    let noise_out = rms(f, 2 * RATE);
    let tone_out = rms(4 * RATE, n);

    let noise_in = rms_level(&input[0..2 * RATE]);
    let tone_in = rms_level(&input[4 * RATE..n]);

    println!(
        "{label}: noise: in {:.3} -> out {:.5} ({:.1} dB) | tone: in {:.3} -> out {:.3}",
        noise_in,
        noise_out,
        20.0 * (noise_out / noise_in.max(1e-9)).log10(),
        tone_in,
        tone_out
    );

    // Noise floor crushed well below the input (Krisp-like gate).
    assert!(noise_out < noise_in * 0.03, "{label}: noise not suppressed: {noise_out} vs {noise_in}");
    // Speech-like tone survives (not cut to silence).
    assert!(tone_out > tone_in * 0.3, "{label}: tone attenuated too far: {tone_out} vs {tone_in}");
}

#[test]
fn fastenhancer_tiers_suppress_noise_preserve_tone() {
    match FastEnhancerDenoiser::new() {
        Some(mut fe) => run_tier_contract("M (Ultra)", &mut fe),
        None => eprintln!("fe Medium unavailable (no AVX2+FMA3+F16C) — skipping"),
    }
    match FastEnhancerDenoiser::new_small() {
        Some(mut fe) => run_tier_contract("S (Ligera)", &mut fe),
        None => eprintln!("fe Small unavailable (no AVX2+FMA3+F16C) — skipping"),
    }
}

/// Full-chain check on the hard 36 s input: S must reproduce the fe-ab probe
/// numbers (floor ≈ −67.6 dBFS, voice ≈ −12.3 dBFS) and M must mute the
/// quiet windows (< −100). Run with `-- --ignored --nocapture`.
#[test]
#[ignore = "heavy: processes the 36 s hard input through 3 chains"]
fn fe_tiers_chain_hard36() {
    use lumen_voice::audio::{NoiseSuppressor, SuppressorModel};
    use std::io::Read;

    let path = "../../samples/harness_fe_hard36_input.wav";
    let mut f = std::fs::File::open(path).expect("open hard36 input");
    let mut b = Vec::new();
    f.read_to_end(&mut b).unwrap();
    let input: Vec<i16> = b[44..]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    println!("input: {} samples ({:.1} s)", input.len(), input.len() as f64 / RATE as f64);

    let metrics = |label: &str, out: &[i16]| {
        let rms = |x: &[i16]| -> f64 {
            (x.iter().map(|&s| (s as i64) * (s as i64)).sum::<i64>() as f64 / x.len() as f64).sqrt()
        };
        let win = 4800usize;
        let mut owins: Vec<f64> = out.chunks(win).map(rms).collect();
        owins.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let floor = owins[owins.len() / 10];
        let voice = owins[(owins.len() as f64 * 0.9) as usize];
        let dbfs = |r: f64| if r > 0.0 { 20.0 * (r / 32768.0).log10() } else { -120.0 };
        println!("{label}: floor {:.1} dBFS | voice p90 {:.1} dBFS", dbfs(floor), dbfs(voice));
        (dbfs(floor), dbfs(voice))
    };

    for (label, model, expect_floor_lt, expect_voice_range) in [
        ("S (Ligera)", SuppressorModel::FastEnhancerS, -60.0, (-15.0, -10.0)),
        ("M (Ultra)", SuppressorModel::FastEnhancerM, -100.0, (-15.0, -10.0)),
        ("NS-only", SuppressorModel::NsOnly, -45.0, (-15.0, -10.0)),
    ] {
        let mut ns = NoiseSuppressor::with_model_and_aec(model, false);
        let mut out_all: Vec<i16> = Vec::with_capacity(input.len());
        for frame in input.chunks_exact(FRAME) {
            out_all.extend(ns.process(frame));
        }
        assert!(
            out_all.len() >= input.len() - FRAME,
            "{label}: chain output too short: {} vs {}",
            out_all.len(),
            input.len()
        );
        let (floor, voice) = metrics(label, &out_all);
        assert!(floor < expect_floor_lt, "{label}: floor {floor:.1} not < {expect_floor_lt}");
        assert!(
            voice > expect_voice_range.0 && voice < expect_voice_range.1,
            "{label}: voice {voice:.1} outside {:?}",
            expect_voice_range
        );
    }
}
