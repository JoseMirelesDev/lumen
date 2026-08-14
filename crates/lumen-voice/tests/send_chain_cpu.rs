//! Per-stage CPU bench for the voice send chain + empty playout tick.
//!
//! Measures µs per 20 ms frame for each stage in isolation so the send-chain
//! cost can be attributed stage-by-stage (deliverable #1 of the CPU slice):
//!
//!   capture resample (identity) → APM (AEC3 on/off variants) → limiter →
//!   rms → opus encode (silence/speech, DTX on/off) → full process_gated →
//!   playout empty tick (push silence).
//!
//! Run: `cargo test --release -p lumen-voice --test send_chain_cpu -- --ignored --nocapture`

use lumen_voice::audio::{rms_level, AudioOutput, NoiseSuppressor};
use std::time::Instant;
use webrtc_audio_processing::{
    config::{
        AdaptiveDigital, Config, EchoCanceller, FixedDigital, GainController, GainController2,
        HighPassFilter, NoiseSuppression, NoiseSuppressionLevel,
    },
    Processor,
};

const RATE: u32 = 48_000;
const FRAME: usize = 960;

/// The production APM config (mirror of `audio.rs::apm_config`).
fn apm_config_full() -> Config {
    Config {
        echo_canceller: Some(EchoCanceller::Full { stream_delay_ms: None }),
        high_pass_filter: Some(HighPassFilter { apply_in_full_band: true }),
        noise_suppression: Some(NoiseSuppression {
            level: NoiseSuppressionLevel::VeryHigh,
            analyze_linear_aec_output: false,
        }),
        gain_controller: Some(GainController::GainController2(GainController2 {
            input_volume_controller_enabled: false,
            adaptive_digital: Some(AdaptiveDigital {
                headroom_db: 5.0,
                max_gain_db: 50.0,
                initial_gain_db: 15.0,
                max_gain_change_db_per_second: 6.0,
                max_output_noise_level_dbfs: -50.0,
            }),
            fixed_digital: FixedDigital { gain_db: 0.0 },
        })),
        ..Config::default()
    }
}

/// Same chain minus the echo canceller (the aec_enabled=false case).
fn apm_config_no_aec() -> Config {
    Config { echo_canceller: None, ..apm_config_full() }
}

fn silence_frame() -> Vec<i16> {
    vec![0i16; FRAME]
}

/// Speech-like frame: 220 Hz tone with 8 Hz AM at ~-20 dBFS.
fn speech_frame(i: usize) -> Vec<i16> {
    (0..FRAME)
        .map(|n| {
            let t = (i * FRAME + n) as f64 / RATE as f64;
            let am = 0.7 + 0.3 * (2.0 * std::f64::consts::PI * 8.0 * t).sin();
            (am * 3000.0 * (2.0 * std::f64::consts::PI * 220.0 * t).sin()) as i16
        })
        .collect()
}

/// Deterministic white noise at a given RMS (xorshift), the real-mic floor
/// case: -57 dBFS ≈ 0.0014 RMS (measured on the live input) is NOT digital
/// silence — opus's DTX VAD and the NS analysis treat it as active signal.
fn noise_frames(n: usize, rms: f32) -> Vec<Vec<i16>> {
    let mut state = 0x1234_5678u32;
    let raw_std = 74.0; // std of uniform [-128, 127]
    let amp = (rms * 32767.0) / raw_std;
    (0..n)
        .map(|_| {
            (0..FRAME)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    (((state >> 8) as i16) as f32 * amp)
                        .round()
                        .clamp(i16::MIN as f32, i16::MAX as f32) as i16
                })
                .collect()
        })
        .collect()
}

fn bench_apm(label: &str, config: Config, frames: &[Vec<i16>]) {
    let p = Processor::new(RATE).unwrap();
    p.set_config(config);
    let mut buf = [0f32; 480];
    let mut sink = 0u64;
    // warmup: let AEC3/NS internal state settle (filter adaptation, AGC)
    for f in &frames[..frames.len().min(200)] {
        for c in f.chunks_exact(480) {
            for (i, s) in c.iter().enumerate() {
                buf[i] = *s as f32 / 32768.0;
            }
            p.process_capture_frame([&mut buf]).unwrap();
        }
    }
    let t0 = Instant::now();
    let n = frames.len();
    for f in frames {
        for c in f.chunks_exact(480) {
            for (i, s) in c.iter().enumerate() {
                buf[i] = *s as f32 / 32768.0;
            }
            p.process_capture_frame([&mut buf]).unwrap();
        }
    }
    let us = t0.elapsed().as_micros() as f64 / n as f64;
    println!("  {label:44} {us:8.1} µs/frame ({:.2}% of 1 core)", us / 20_000.0 * 100.0);
    let _ = sink;
}

fn bench_encoder(label: &str, frames: &[Vec<i16>], dtx: bool) {
    let mut enc = lumen_voice::audio::OpusEncoder::new().unwrap();
    if dtx {
        enc.set_dtx(true);
    }
    // warmup
    for f in &frames[..frames.len().min(100)] {
        let _ = enc.encode(f);
    }
    let t0 = Instant::now();
    let n = frames.len();
    let mut bytes = 0usize;
    for f in frames {
        bytes += enc.encode(f).map(|p| p.len()).unwrap_or(0);
    }
    let us = t0.elapsed().as_micros() as f64 / n as f64;
    println!(
        "  {label:44} {us:8.1} µs/frame ({:.2}% of 1 core)  avg pkt {} B",
        us / 20_000.0 * 100.0,
        bytes / n
    );
}

fn bench_ns_chain(label: &str, frames: &[Vec<i16>]) {
    let mut ns = NoiseSuppressor::new();
    let mut sink = 0i32;
    for f in &frames[..frames.len().min(200)] {
        let out = ns.process_gated(f).unwrap();
        sink = sink.wrapping_add(out.iter().map(|s| *s as i32).sum::<i32>());
    }
    let t0 = Instant::now();
    let n = frames.len();
    for f in frames {
        let out = ns.process_gated(f).unwrap();
        sink = sink.wrapping_add(out.iter().map(|s| *s as i32).sum::<i32>());
    }
    let us = t0.elapsed().as_micros() as f64 / n as f64;
    println!("  {label:44} {us:8.1} µs/frame ({:.2}% of 1 core)", us / 20_000.0 * 100.0);
    let _ = sink;
}

/// Full send chain in one loop, timed per stage — mirrors the production
/// send task (process_gated then encode) on the SAME signal the app sees.
fn bench_full_chain(label: &str, frames: &[Vec<i16>], complexity: i32) {
    let mut ns = NoiseSuppressor::new();
    let mut enc = lumen_voice::audio::OpusEncoder::new().unwrap();
    if complexity >= 0 {
        enc.set_complexity(complexity as i32);
    }
    let mut sink = 0i32;
    let mut ns_us = 0u64;
    let mut enc_us = 0u64;
    let mut bytes = 0usize;
    for f in &frames[..frames.len().min(200)] {
        let out = ns.process_gated(f).unwrap();
        let _ = enc.encode(&out);
        sink = sink.wrapping_add(out.iter().map(|s| *s as i32).sum::<i32>());
    }
    let t0 = std::time::Instant::now();
    let n = frames.len();
    for f in frames {
        let t1 = std::time::Instant::now();
        let out = ns.process_gated(f).unwrap();
        let t2 = std::time::Instant::now();
        bytes += enc.encode(&out).map(|p| p.len()).unwrap_or(0);
        let t3 = std::time::Instant::now();
        ns_us += t2.duration_since(t1).as_micros() as u64;
        enc_us += t3.duration_since(t2).as_micros() as u64;
        sink = sink.wrapping_add(out.iter().map(|s| *s as i32).sum::<i32>());
    }
    let wall = t0.elapsed().as_micros() as f64 / n as f64;
    println!(
        "  {label:44} {wall:8.1} µs/frame (ns {:.0} + enc {:.0} = {:.0} µs, {:.2}% of 1 core, avg pkt {} B)",
        ns_us as f64 / n as f64,
        enc_us as f64 / n as f64,
        (ns_us + enc_us) as f64 / n as f64,
        wall / 20_000.0 * 100.0,
        bytes / n
    );
    let _ = sink;
}

fn bench_playout_tick(label: &str, frames: usize) {
    // Empty-tick cost: what the playout task does per 20 ms with 0 peers.
    // (NetEQ loop skipped, push(silence) runs.) Measured in-process.
    let out = AudioOutput::new();
    let silence = vec![0i16; FRAME];
    // No stream started → push is a no-op (state None); that's the headless
    // floor. With a real stream it adds the LinearResampler + extend.
    let t0 = Instant::now();
    for _ in 0..frames {
        out.push(&silence);
    }
    let us = t0.elapsed().as_micros() as f64 / frames as f64;
    println!(
        "  {label:44} {us:8.1} µs/tick ({:.2}% of 1 core, headless no-op floor)",
        us / 20_000.0 * 100.0
    );
}

#[test]
#[ignore = "manual per-stage CPU bench (release build)"]
fn send_chain_cpu() {
    println!("=== send chain per-stage CPU (µs per 20 ms frame, release) ===");
    let n_frames = 2000; // 40 s of audio
    let silence: Vec<Vec<i16>> = (0..n_frames).map(|_| silence_frame()).collect();
    let speech: Vec<Vec<i16>> = (0..n_frames).map(speech_frame).collect();

    println!("-- APM process_capture_frame (2×480 blocks per 20 ms frame) --");
    bench_apm("APM full (AEC3+HPF+NS+GC2) — silence", apm_config_full(), &silence);
    bench_apm("APM no-AEC (HPF+NS+GC2) — silence", apm_config_no_aec(), &silence);
    bench_apm("APM full (AEC3+HPF+NS+GC2) — speech", apm_config_full(), &speech);
    bench_apm("APM no-AEC (HPF+NS+GC2) — speech", apm_config_no_aec(), &speech);

    let noise57 = noise_frames(n_frames, 0.0014); // real mic floor (-57 dBFS)
    let noise40 = noise_frames(n_frames, 0.01); // -40 dBFS

    println!("-- opus encode (DTX on = production) --");
    bench_encoder("opus digital silence", &silence, true);
    bench_encoder("opus noise -57dB (mic floor)", &noise57, true);
    bench_encoder("opus noise -40dB", &noise40, true);
    bench_encoder("opus speech", &speech, true);

    println!("-- full shipped send chain --");
    bench_ns_chain("process_gated — digital silence", &silence);
    bench_ns_chain("process_gated — noise -57dB (mic floor)", &noise57);
    bench_ns_chain("process_gated — speech", &speech);

    println!("-- full send chain (NS -> opus encode), the app's real signal --");
    bench_full_chain("noise -57dB, complexity 10 (prod)", &noise57, -1);
    bench_full_chain("noise -57dB, complexity 6", &noise57, 6);
    bench_full_chain("noise -57dB, complexity 4", &noise57, 4);
    bench_full_chain("digital silence, complexity 10", &silence, -1);
    bench_full_chain("speech, complexity 10 (prod)", &speech, -1);
    bench_full_chain("speech, complexity 6", &speech, 6);

    println!("-- playout --");
    bench_playout_tick("empty-tick push(silence)", n_frames);
    println!("-- rms --");
    let t0 = Instant::now();
    for f in &speech {
        let _ = rms_level(f);
    }
    let us = t0.elapsed().as_micros() as f64 / n_frames as f64;
    println!("  rms_level {:>8.1} µs/frame", us);
    println!("=== done ===");
}
