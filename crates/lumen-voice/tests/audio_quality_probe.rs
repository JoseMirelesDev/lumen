//! Audio-quality probe: quiet speech + background noise + speaker echo through
//! the full send path, with audible WAV artifacts.
//!
//! Simulates the reported scenario on a real sample:
//!   - Near-end speech (YOUR voice): `samples/real_speech_es_48k.wav` (first
//!     5 s skipped — that opening is low-SNR hiss the denoiser correctly
//!     gates) scaled to a quiet-mic level (RMS ≈ -28 dBFS) — exercises the
//!     leveler's boost.
//!   - Far-end (the other side, played on your speakers): NON-periodic
//!     synthetic speech (AM harmonic stack) — decorrelated from the near-end
//!     so AEC3 cancels only the echo, exactly like a real call, and not
//!     periodic (a looped recording makes the AEC3's delay estimator
//!     re-search at every repetition — a probe artifact). Active from t = 0.
//!   - Background noise: deterministic xorshift white noise (RMS 0.01).
//!   - Echo: the far-end picked up by the open mic (×0.4) delayed 50 ms.
//!
//! Scenarios:
//!   A. speech + noise + echo, AEC render fed -> `probe_send_output.wav`
//!      (the input DUPLICATED — two copies concatenated — processed in ONE
//!      pass: half 1 = cold start with the AEC3 convergence window, half 2 =
//!      the same audio extended with the chain already calibrated), plus
//!      `probe_send_output_calibrated.wav` (the steady-state half-1 output
//!      from t=2.5 s repeated 3x, crossfaded — what the pipeline sounds like
//!      once everything is warm).
//!   B. echo only, AEC render fed (cancelled)  -> `probe_echo_aec_on.wav`
//!   C. echo only, no render (uncancelled)     -> `probe_echo_aec_off.wav`
//!   D. noise only                             -> measures the noise floor
//!
//! WAVs (48 kHz mono i16) are written to `samples/` at the repo root, next to
//! the source sample. `probe_mic_input.wav` = the raw mic mix of scenario A.
//!
//! Run: `cargo test -p lumen-voice --test audio_quality_probe -- --ignored --nocapture`

use lumen_voice::audio::{rms_level, NoiseSuppressor};
use std::io::Read;

const RATE: u32 = 48_000;
const FRAME: usize = 960; // 20 ms @ 48 kHz

fn load_speech() -> Vec<i16> {
    let mut file = std::fs::File::open(
        std::env::current_dir()
            .unwrap()
            .join("../../samples/real_speech_es_48k.wav"),
    )
    .expect("samples/real_speech_es_48k.wav not found — run from crates/lumen-voice/");
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).unwrap();
    // Skip 44-byte WAV header, read i16 LE samples.
    buf[44..]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn scale(v: &[i16], g: f32) -> Vec<i16> {
    v.iter()
        .map(|&s| ((s as f32) * g).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
        .collect()
}

/// Deterministic pseudo-random white noise (xorshift).
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

fn mix(a: &[i16], b: &[i16]) -> Vec<i16> {
    a.iter()
        .zip(b)
        .map(|(&x, &y)| (x as i32 + y as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16)
        .collect()
}

fn write_wav(path: &str, pcm: &[i16]) {
    let mut buf = Vec::with_capacity(44 + pcm.len() * 2);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + pcm.len() as u32 * 2).to_le_bytes());
    buf.extend_from_slice(b"WAVEfmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&RATE.to_le_bytes());
    buf.extend_from_slice(&(RATE * 2).to_le_bytes()); // byte rate
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(pcm.len() as u32 * 2).to_le_bytes());
    for &s in pcm {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, &buf).unwrap_or_else(|e| panic!("write {path}: {e}"));
}

#[test]
#[ignore = "manual probe: writes WAVs to samples/ and prints a report"]
fn audio_quality_probe() {
    let mut near = load_speech(); // YOUR voice: the real Spanish sample
    // Skip the file's first 5 s (low-SNR hiss opening that DeepFilterNet
    // correctly gates as noise) so the demo starts on real speech.
    near.drain(..5 * RATE as usize);
    // The WAV may not be 20 ms aligned; drop the trailing partial frame.
    near.truncate(near.len() - near.len() % FRAME);
    let n_frames = near.len() / FRAME;
    assert!(n_frames > 0, "sample too short");

    // Far-end (the OTHER side, played on your speakers): NON-periodic
    // synthetic speech (AM harmonic stack — reads as speech to the models).
    // A periodically looped recording makes the AEC3's delay estimator
    // re-search at every repetition (the echo correlates at multiple lags) —
    // a probe artifact that keeps re-entering the transparent window and
    // passing the echo; real far-end audio is not periodic. Active from t = 0
    // (the realistic open-mic case: you join a channel where the far end is
    // already talking).
    let render: Vec<i16> = (0..near.len())
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

    // Quiet-mic near-end speech (RMS ~0.0407 ≈ -28 dBFS, the same level as the
    // agc_probe's speech.wav) — the leveler's boost case.
    let quiet = scale(&near, 0.5);
    // Speaker echo picked up by the open mic: attenuated, delayed 50 ms.
    let echo_delay = 50 * RATE as usize / 1000; // 2400 samples
    let mut echo = vec![0i16; echo_delay];
    echo.extend(scale(&render[..render.len() - echo_delay], 0.4));
    // Room background noise, quiet-room level (RMS 0.01 ≈ -40 dBFS). The
    // generator's raw RMS depends on the cast semantics, so normalize to the
    // target level.
    let noise_raw = xorshift_noise(near.len(), 4);
    let noise = scale(&noise_raw, 0.01 / rms_level(&noise_raw));

    let out_dir = std::env::current_dir().unwrap().join("../../samples");
    std::fs::create_dir_all(&out_dir).unwrap();

    println!("=== Audio-quality probe (samples/real_speech_es_48k.wav) ===");
    println!("Speech quiet-mic : RMS {:.4} ({:.1} dBFS)",
        rms_level(&quiet), 20.0 * rms_level(&quiet).log10());
    println!("Echo (in mic)    : RMS {:.4} ({:.1} dBFS)",
        rms_level(&echo), 20.0 * rms_level(&echo).log10());
    println!("Background noise : RMS {:.4} ({:.1} dBFS)",
        rms_level(&noise), 20.0 * rms_level(&noise).log10());

    // ---- Scenario A: speech + noise + echo, full chain, AEC render fed ----
    // The input is DUPLICATED (two copies concatenated) and processed in ONE
    // pass over the extended audio: the first half shows the cold start
    // (AEC3 convergence window, model warmup, leveler ramp), the second half
    // is the same audio continued with everything already calibrated (AEC3
    // converged, model warm, leveler settled).
    let mic_a = mix(&mix(&quiet, &noise), &echo);
    let mic_a_rms = rms_level(&mic_a);
    write_wav(out_dir.join("probe_mic_input.wav").to_str().unwrap(), &mic_a);
    let mic_2x: Vec<i16> = [&mic_a[..], &mic_a[..]].concat();
    let render_2x: Vec<i16> = [&render[..], &render[..]].concat();

    let mut ns = NoiseSuppressor::new();
    let mut out_a = Vec::with_capacity(mic_2x.len());
    let mut half_metrics = Vec::with_capacity(2);
    let mut speech_rms_sum = 0f64;
    let mut speech_frames = 0u32;
    let mut boosted_frames = 0u32;
    let mut out_peak = 0f32;
    let mut lsnr_sum = 0f64;
    let mut lsnr_frames = 0u32;
    let half_len = mic_a.len() / FRAME;
    for (i, f) in mic_2x.chunks_exact(FRAME).enumerate() {
        // Feed the far-end reference (10 ms chunks) before each capture frame,
        // exactly like the send loop in client.rs.
        for c in render_2x[i * FRAME..(i + 1) * FRAME].chunks(480) {
            ns.process_render_frame(c);
        }
        let out = ns.process(f);
        assert_eq!(out.len(), FRAME);
        out_a.extend_from_slice(&out);
        out_peak = out_peak.max(out.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as f32);
        if ns.speech_detected() {
            speech_frames += 1;
            speech_rms_sum += rms_level(&out) as f64;
        }
        if (i + 1) % half_len == 0 {
            half_metrics.push((speech_frames, speech_rms_sum, out_peak));
            println!(
                "half {}: {speech_frames}/{} speech frames, peak {out_peak:.0}",
                half_metrics.len(), half_len
            );
            speech_rms_sum = 0.0;
            speech_frames = 0;
            out_peak = 0.0;
        }
    }
    write_wav(out_dir.join("probe_send_output.wav").to_str().unwrap(), &out_a);
    let (sf1, sr1, pk1) = half_metrics[0];
    let (sf2, sr2, pk2) = half_metrics[1];
    let avg_speech_out = sr2 / sf2.max(1) as f64;

    // Calibrated loop: a steady-state passage from half 1 anchored to REAL
    // sentence boundaries, repeated 3x with 10 ms crossfades. Detected on the
    // INPUT (the output's pause frames carry the leveler's decaying gain,
    // which muddies pause detection): start right after the first pause at/after
    // t = 6.5 s (AEC converged, leveler settled, the file's hissy opening
    // skipped), end at the last pause at/before t = 33 s (before the quiet
    // tail). Starting mid-phrase sounded "descalibrado" and a tail-end region
    // dropped the volume at the end of every copy.
    let pass1 = &out_a[..n_frames * FRAME];
    let in_low = |i: usize| rms_level(&mic_a[i * FRAME..(i + 1) * FRAME]) < 0.015;
    let mut cal_start = 325; // t = 6.5 s
    while cal_start < n_frames && !in_low(cal_start) {
        cal_start += 1;
    }
    cal_start = (cal_start + 1).min(n_frames.saturating_sub(1));
    if cal_start > 1600 {
        cal_start = 325; // no pause found before the tail — fallback
    }
    let mut cal_end = 1650; // t = 33 s
    while cal_end > cal_start && !in_low(cal_end) {
        cal_end -= 1;
    }
    if cal_end <= cal_start + 100 {
        cal_end = 1650; // no usable pause — fallback
    }
    let region = &pass1[cal_start * FRAME..cal_end * FRAME];
    let fade = 480; // 10 ms
    let mut cal_out = Vec::with_capacity(region.len() * 3);
    for _ in 0..3 {
        if cal_out.is_empty() {
            cal_out.extend_from_slice(region);
        } else {
            let start = cal_out.len() - fade;
            for j in 0..fade {
                let a = cal_out[start + j] as f32;
                let b = region[j] as f32;
                let t = j as f32 / fade as f32;
                cal_out[start + j] = (a * (1.0 - t) + b * t).round() as i16;
            }
            cal_out.extend_from_slice(&region[fade..]);
        }
    }
    write_wav(out_dir.join("probe_send_output_calibrated.wav").to_str().unwrap(), &cal_out);
    let cal_rms = rms_level(&cal_out);
    let cal_peak = cal_out.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    println!();
    println!("--- A: quiet speech + noise + echo (AEC on, input duplicated, ONE pass) ---");
    println!("Mic input RMS      : {mic_a_rms:.4}");
    println!("HALF 1 (cold)      : {sf1}/{} speech frames, peak {pk1:.0}",
        half_len);
    println!("HALF 2 (calibrated): {sf2}/{} speech frames, peak {pk2:.0}",
        half_len);
    println!("  (half 2 = the duplicated audio continued with the chain warm: no AEC");
    println!("   initial window; the GC2 keeps the level stable)");
    println!("Calibrated loop    : {:.1} s (t={:.1}s..{:.1}s x3), RMS {cal_rms:.4}, peak {cal_peak}",
        cal_out.len() as f64 / RATE as f64,
        cal_start as f64 * 20.0 / 1000.0,
        cal_end as f64 * 20.0 / 1000.0);
    println!("Output peak (p2)   : {:.1} (<= -1 dBFS: {})",
        pk2, pk2 <= 10f32.powf(-1.0 / 20.0) * 32767.0 + 1.0);
    println!("Avg speech out RMS (half 2): {avg_speech_out:.4} ({:.1} dBFS) — audible",
        20.0 * avg_speech_out.log10());

    // ---- Scenario B/C: echo only, AEC on vs off ----
    let mut out_b = Vec::with_capacity(echo.len());
    let mut ns_b = NoiseSuppressor::new();
    for (i, f) in echo.chunks_exact(FRAME).enumerate() {
        for c in render[i * FRAME..(i + 1) * FRAME].chunks(480) {
            ns_b.process_render_frame(c);
        }
        out_b.extend_from_slice(&ns_b.process(f));
    }
    write_wav(out_dir.join("probe_echo_aec_on.wav").to_str().unwrap(), &out_b);

    let mut out_c = Vec::with_capacity(echo.len());
    let mut ns_c = NoiseSuppressor::new(); // render never fed -> echo survives
    for f in echo.chunks_exact(FRAME) {
        out_c.extend_from_slice(&ns_c.process(f));
    }
    write_wav(out_dir.join("probe_echo_aec_off.wav").to_str().unwrap(), &out_c);

    let echo_in = rms_level(&echo);
    let on_overall = rms_level(&out_b);
    let on_last10 = rms_level(&out_b[out_b.len() - 480_000..]); // last 10 s
    let off_overall = rms_level(&out_c);
    let atten = |out: f32| 20.0 * (echo_in / out.max(1e-9)).log10();
    println!();
    println!("--- B/C: echo only (far-end speech played on speakers) ---");
    println!("Echo in mic RMS    : {echo_in:.4}");
    println!("AEC ON  out RMS    : {on_overall:.4}  ({:.1} dB attenuation, last 10 s: {:.1} dB)",
        atten(on_overall), atten(on_last10));
    println!("AEC OFF out RMS    : {off_overall:.4}  ({:.1} dB attenuation)", atten(off_overall));

    // ---- Scenario D: noise only ----
    let mut out_d = Vec::with_capacity(noise.len());
    let mut ns_d = NoiseSuppressor::new();
    for f in noise.chunks_exact(FRAME) {
        out_d.extend_from_slice(&ns_d.process(f));
    }
    let noise_in = rms_level(&noise);
    let noise_out = rms_level(&out_d);
    println!();
    println!("--- D: background noise only (must NOT be amplified) ---");
    println!("Noise in RMS : {noise_in:.4} -> out RMS {noise_out:.4} ({:.1} dB suppression)",
        20.0 * (noise_in / noise_out.max(1e-9)).log10());

    // ---- Scenario E: denoiser WITHOUT the echo (no far-end, no render) ----
    // The send path with only near-end speech + noise: the AEC gets no render
    // and passes the capture; DeepFilterNet + leveler do their thing. Lets the
    // listener hear the denoiser's own sound, uncontaminated by the AEC/echo.
    let mic_e = mix(&quiet, &noise);
    write_wav(out_dir.join("probe_noecho_input.wav").to_str().unwrap(), &mic_e);
    let mut out_e = Vec::with_capacity(mic_e.len());
    let mut ns_e = NoiseSuppressor::new();
    for f in mic_e.chunks_exact(FRAME) {
        out_e.extend_from_slice(&ns_e.process(f));
    }
    write_wav(out_dir.join("probe_denoised_noecho.wav").to_str().unwrap(), &out_e);
    println!();
    println!("--- E: denoiser without echo (speech + noise, no far-end) ---");
    println!("Input RMS {:.4} -> output RMS {:.4}, peak {}",
        rms_level(&mic_e), rms_level(&out_e),
        out_e.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0));

    // ---- Sanity asserts (loose; the printed numbers are the evidence) ----
    assert!(pk2 <= 10f32.powf(-1.0 / 20.0) * 32767.0 + 1.0,
        "send path must never exceed -1 dBFS: peak {pk2}");
    assert!(sf2 > 100,
        "the calibrated half must keep detecting speech: {sf2} speech frames");
    assert!(cal_peak > 5000,
        "calibrated loop must contain boosted speech: peak {cal_peak}");
    assert!(on_last10 < off_overall * 0.5,
        "AEC must substantially reduce echo: on {on_overall:.4} vs off {off_overall:.4}");
    assert!(noise_out < noise_in * 0.3,
        "background noise must not be amplified: {noise_in} -> {noise_out}");
    assert!(avg_speech_out > 0.04,
        "quiet speech must be boosted to an audible level: {avg_speech_out:.4}");

    println!();
    println!("WAVs written to {out_dir:?}:");
    println!("  probe_mic_input.wav   — raw mic mix (quiet speech + noise + echo)");
    println!("  probe_send_output.wav — full chain, input duplicated + ONE pass:");
    println!("                         half 1 cold start, half 2 calibrated");
    println!("  probe_send_output_calibrated.wav — steady-state output (t=2.5s..end)");
    println!("                         repeated 3x with crossfaded seams");
    println!("  probe_echo_aec_on.wav / probe_echo_aec_off.wav — echo cancelled vs not");
    println!("=== end probe ===");
}
