//! Preview of the PRODUCTION send chain: `NoiseSuppressor::new()` (the
//! committed default — AEC3 transparent patch + NS VeryHigh + GainController2
//! + limiter) on the full-level Spanish speech sample + synthetic echo.
//!
//! Writes:
//!   samples/prod_chain_input.wav  — capture as fed to the chain (speech +
//!                                   echo ×0.4 delayed 50 ms)
//!   samples/prod_chain_preview.wav — the chain's send output
//!
//! Run: `cargo test -p lumen-voice --test prod_chain_preview -- --ignored --nocapture`

use lumen_voice::audio::{rms_level, NoiseSuppressor};

const RATE: u32 = 48_000;
const FRAME: usize = 960; // 20 ms @ 48 kHz

fn scale(v: &[i16], g: f32) -> Vec<i16> {
    v.iter()
        .map(|s| (*s as f32 * g).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
        .collect()
}

fn mix(a: &[i16], b: &[i16]) -> Vec<i16> {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| x.saturating_add(*y))
        .collect()
}

/// Deterministic pseudo-random white noise (xorshift) — same generator as
/// the audio_quality_probe.
fn xorshift_noise(samples: usize, scale: i16) -> Vec<i16> {
    let mut state = 0x1234_5678u32;
    (0..samples)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            ((state >> 8) as i16) / scale
        })
        .collect()
}

fn load(path: &str) -> Vec<i16> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(std::env::current_dir().unwrap().join(path)).unwrap();
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).unwrap();
    buf[44..]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn write_wav(path: &str, pcm: &[i16]) {
    let n = pcm.len() as u32;
    let mut buf = Vec::with_capacity(44 + pcm.len() * 2);
    buf.extend(b"RIFF");
    buf.extend((36 + n * 2).to_le_bytes());
    buf.extend(b"WAVEfmt ");
    buf.extend(16u32.to_le_bytes());
    buf.extend(1u16.to_le_bytes()); // PCM
    buf.extend(1u16.to_le_bytes()); // mono
    buf.extend(RATE.to_le_bytes());
    buf.extend((RATE * 2).to_le_bytes());
    buf.extend(2u16.to_le_bytes());
    buf.extend(16u16.to_le_bytes());
    buf.extend(b"data");
    buf.extend((n * 2).to_le_bytes());
    for s in pcm {
        buf.extend(s.to_le_bytes());
    }
    std::fs::write(path, &buf).unwrap_or_else(|e| panic!("write {path}: {e}"));
}

#[test]
#[ignore = "manual preview: writes the production chain's output WAV"]
fn prod_chain_preview() {
    let mut speech = load("../../samples/real_speech_es_48k.wav");
    speech.truncate(speech.len() - speech.len() % FRAME);
    let n_frames = speech.len() / FRAME;
    println!("=== production chain preview ===");
    println!("sample: {:.1} s, {} frames @ {RATE} Hz, RMS {:.4}",
        n_frames as f64 * 0.02, n_frames, rms_level(&speech));

    // Far-end: the same non-periodic synthetic speech as the A/B harness.
    let render: Vec<i16> = (0..speech.len())
        .map(|i| {
            let t = i as f64 / RATE as f64;
            let mut v = 0.0;
            for (k, a) in [(1, 1.0), (2, 0.5), (3, 0.33), (4, 0.25), (5, 0.2)] {
                v += a * (2.0 * std::f64::consts::PI * 150.0 * k as f64 * t).sin();
            }
            let am = 0.7 + 0.3 * (2.0 * std::f64::consts::PI * 8.0 * t).sin();
            (v * am * 2000.0) as i16
        })
        .collect();

    // Capture: speech + echo (far-end ×0.4, 50 ms delay).
    let mut echo = vec![0i16; 2400];
    echo.extend(scale(&render[..render.len() - 2400], 0.4));
    let capture = mix(&speech, &echo);
    write_wav(
        "../../samples/prod_chain_input.wav",
        &capture,
    );
    println!("input written: samples/prod_chain_input.wav (echo RMS {:.4})",
        rms_level(&echo));

    let mut ns = NoiseSuppressor::new();
    let mut out = Vec::with_capacity(capture.len());
    for (i, frame) in capture.chunks_exact(FRAME).enumerate() {
        // Feed this frame's render (2 × 480) before the capture, like the
        // receive path does.
        ns.process_render_frame(&render[i * FRAME..(i + 1) * FRAME]);
        out.extend(ns.process(frame));
    }
    write_wav("../../samples/prod_chain_preview.wav", &out);
    println!("output written: samples/prod_chain_preview.wav");
    println!("output RMS {:.4} ({:.1} dBFS), peak {:?} — same scenario as the A/B chains",
        rms_level(&out), 20.0 * rms_level(&out).log10(),
        out.iter().map(|s| s.unsigned_abs()).max());

    // ---- Hard scenario: the probe's worst case (quiet mic + noise + echo) ----
    // Same input as audio_quality_probe scenario A: the sample with its
    // low-SNR opening skipped, voice x0.5, room noise at RMS 0.01, echo.
    let mut near = speech.clone();
    near.drain(..5 * RATE as usize);
    near.truncate(near.len() - near.len() % FRAME);
    let quiet = scale(&near, 0.5);
    let echo_delay = 50 * RATE as usize / 1000;
    let mut echo = vec![0i16; echo_delay];
    echo.extend(scale(&render[..render.len() - echo_delay], 0.4));
    let noise_raw = xorshift_noise(near.len(), 4);
    let noise = scale(&noise_raw, 0.01 / rms_level(&noise_raw));
    let mic = mix(&mix(&quiet, &noise), &echo);
    write_wav("../../samples/prod_chain_hard_input.wav", &mic);
    println!("hard input written: samples/prod_chain_hard_input.wav");
    println!("  (quiet voice RMS {:.4}, noise RMS {:.4}, echo RMS {:.4})",
        rms_level(&quiet), rms_level(&noise), rms_level(&echo));

    let mut ns = NoiseSuppressor::new();
    let mut out = Vec::with_capacity(mic.len());
    for (i, frame) in mic.chunks_exact(FRAME).enumerate() {
        ns.process_render_frame(&render[i * FRAME..(i + 1) * FRAME]);
        out.extend(ns.process(frame));
    }
    write_wav("../../samples/prod_chain_preview_hard.wav", &out);
    println!("hard output written: samples/prod_chain_preview_hard.wav");
    println!("output RMS {:.4} ({:.1} dBFS), peak {:?}",
        rms_level(&out), 20.0 * rms_level(&out).log10(),
        out.iter().map(|s| s.unsigned_abs()).max());
}
