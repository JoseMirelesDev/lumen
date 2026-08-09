//! Send-chain probe — the winning chain (AEC3 + NS VeryHigh + GainController2
//! + limiter). The GC2 adapts slowly by design (initial gain 0, 3 dB/s — no
//! per-phrase volume surges), so the probe feeds a LONG input (speech.wav x5
//! ≈ 20 s) and asserts on the steady state: audible once settled, stable
//! across windows (no surges).
//!
//! Run: `cargo test -p lumen-voice --test agc_probe -- --nocapture`

use lumen_voice::audio::{rms_level, NoiseSuppressor};
use std::io::Read;

const RATE: u32 = 48_000;
const FRAME: usize = 960; // 20 ms @ 48 kHz

fn load_speech() -> Vec<i16> {
    let mut file = std::fs::File::open(
        std::env::current_dir()
            .unwrap()
            .join("testdata/speech.wav"),
    )
    .expect("testdata/speech.wav not found — run from crates/lumen-voice/");
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).unwrap();
    buf[44..]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

#[test]
fn send_chain_levels() {
    let base = load_speech();
    let speech: Vec<i16> = base.iter().copied().cycle().take(base.len() * 5).collect();
    let n_frames = speech.len() / FRAME;

    println!("=== Send-chain probe (AEC3 + NS + GC2 + limiter) ===");
    println!("Audio: {:.1} s, {n_frames} frames @ {RATE} Hz (speech.wav x5 — the GC2 needs time)",
        n_frames as f64 * 0.02);
    println!();

    let mut ns = NoiseSuppressor::new();
    let mut window_rms: Vec<f32> = Vec::new(); // per 0.5 s speech windows
    let mut in_window_rms: Vec<f32> = Vec::new();
    let mut cur: Vec<f32> = Vec::new();
    let mut in_cur: Vec<f32> = Vec::new();
    let mut speech_frames = 0u32;
    let mut out_peak = 0i16;

    for chunk in speech.chunks_exact(FRAME) {
        let out = ns.process(chunk);
        out_peak = out_peak.max(out.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0).min(32767) as i16);
        if ns.speech_detected() {
            speech_frames += 1;
        }
        cur.push(rms_level(&out));
        in_cur.push(rms_level(chunk));
        if cur.len() >= 25 {
            let avg = cur.iter().sum::<f32>() / cur.len() as f32;
            let in_avg = in_cur.iter().sum::<f32>() / in_cur.len() as f32;
            if avg > 0.02 {
                window_rms.push(avg);
            }
            if in_avg > 0.02 {
                in_window_rms.push(in_avg);
            }
            cur.clear();
            in_cur.clear();
        }
    }

    let half = window_rms.len() / 2;
    let steady = &window_rms[half..];
    let mn = steady.iter().cloned().fold(f32::MAX, f32::min);
    let mx = steady.iter().cloned().fold(0.0f32, f32::max);
    let avg_steady = steady.iter().sum::<f32>() / steady.len().max(1) as f32;
    let in_mn = in_window_rms.iter().cloned().fold(f32::MAX, f32::min);
    let in_mx = in_window_rms.iter().cloned().fold(0.0f32, f32::max);
    let in_ratio = in_mx / in_mn.max(0.0001);
    println!("speech frames detected : {speech_frames}/{n_frames} ({:.0}%)",
        100.0 * speech_frames as f32 / n_frames as f32);
    println!("peak output            : {out_peak} ({:.1} dBFS — limiter ceiling -1)",
        20.0 * (out_peak as f32 / 32767.0).log10());
    println!("steady-state windows   : {} (avg {avg_steady:.4}, min {mn:.4}, max {mx:.4}, ratio {:.2})",
        steady.len(), mx / mn.max(0.0001));

    // Audible once the AGC settles: the steady-state windows at a normal
    // speech level (the input is at ~0.044 RMS; the NS takes ~9 dB, the GC2
    // compensates — expect >= 0.03).
    assert!(
        avg_steady >= 0.03,
        "steady-state output too quiet: {avg_steady:.4} ({:.1} dBFS)",
        20.0 * avg_steady.log10()
    );
    // Stable: the chain must not ADD level variance — the steady-state
    // window ratio stays within 1.5x of the input's own dynamics (the GC2
    // tracks the long-term level; per-phrase surges are gone with the
    // custom leveler).
    let out_ratio = mx / mn.max(0.0001);
    println!("input dynamics ratio   : {in_ratio:.2} (min {in_mn:.4}, max {in_mx:.4})");
    assert!(
        steady.len() >= 5 && out_ratio < in_ratio * 1.5,
        "chain adds level variance: output ratio {out_ratio:.2} vs input {in_ratio:.2}"
    );
    println!("=== end probe ===");
}
