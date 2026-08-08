//! AGC diagnostic probe — measures signal levels and gain through the full
//! send-path chain (AEC3 + DeepFilterNet + SpeechLeveler) on real speech.
//!
//! Verifies the stabilized AGC: quiet speech gets boosted to an audible level
//! while the gain stays stable within contiguous speech (no syllable-level
//! pumping).
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
    // Skip 44-byte WAV header, read i16 LE samples.
    buf[44..]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

#[test]
fn agc_chain_levels() {
    let speech = load_speech();
    let n_frames = speech.len() / FRAME;
    let audio_s = n_frames as f64 * 0.02;

    println!("=== AGC chain probe ===");
    println!("Audio: {audio_s:.1} s, {n_frames} frames @ {RATE} Hz");
    println!();

    // ONE suppressor, processed sequentially — the leveler's state (gain,
    // speech_rms, hangover) carries across frames exactly like production.
    let mut ns = NoiseSuppressor::new();

    println!(
        "{:>5}  {:>8} {:>7} {:>8} {:>7}  {:>5}",
        "frame", "raw_rms", "lsnr", "agc_rms", "gain", "speak"
    );
    println!(
        "{:-<5}  {:-<8} {:-<7} {:-<8} {:-<7}  {:-<5}",
        "", "", "", "", "", ""
    );

    // ---- accumulators ----
    let mut raw_rms_sum = 0.0f64;
    let mut agc_rms_sum = 0.0f64;
    let mut agc_rms_speech_sum = 0.0f64;
    let mut gain_speech_sum = 0.0f64;
    let mut speech_frames = 0u32;
    let mut min_agc_rms = f32::MAX;
    let mut max_agc_rms = 0.0f32;
    let mut min_gain_speech = f32::MAX;
    let mut max_gain_speech = 0.0f32;
    let mut lsnr_min = f32::MAX;
    let mut lsnr_max = f32::MIN;

    // ---- contiguous-speech window tracking (the stability check) ----
    // The FIRST speech window is the one-time attack ramp (unity → target at
    // ~15 dB/s) — expected, not pumping. All later windows must hold the gain
    // within 6 dB (max_gain / min_gain < 2.0).
    let mut first_window = true;
    let mut run_len = 0u32;
    let mut run_min_gain = f32::MAX;
    let mut run_max_gain = 0.0f32;
    let mut worst_gain_ratio = 1.0f32;

    for (i, chunk) in speech.chunks_exact(FRAME).enumerate() {
        let raw_rms = rms_level(chunk);
        let cleaned = ns.process(chunk);
        let agc_rms = rms_level(&cleaned);
        let gain = ns.agc_gain();
        let speak = ns.speech_detected();
        let lsnr = ns.last_lsnr().unwrap_or(f32::NAN);

        if i % 25 == 0 || i < 3 {
            println!(
                "{:>5}  {:>8.4} {:>7.1} {:>8.4} {:>7.2}  {:>5}",
                i,
                raw_rms,
                lsnr,
                agc_rms,
                gain,
                if speak { "YES" } else { "no" }
            );
        }

        raw_rms_sum += raw_rms as f64;
        agc_rms_sum += agc_rms as f64;
        if let Some(l) = ns.last_lsnr() {
            lsnr_min = lsnr_min.min(l);
            lsnr_max = lsnr_max.max(l);
        }
        if speak {
            speech_frames += 1;
            agc_rms_speech_sum += agc_rms as f64;
            gain_speech_sum += gain as f64;
            min_agc_rms = min_agc_rms.min(agc_rms);
            max_agc_rms = max_agc_rms.max(agc_rms);
            min_gain_speech = min_gain_speech.min(gain);
            max_gain_speech = max_gain_speech.max(gain);
            run_len += 1;
            run_min_gain = run_min_gain.min(gain);
            run_max_gain = run_max_gain.max(gain);
        } else {
            close_window(
                run_len,
                run_min_gain,
                run_max_gain,
                &mut first_window,
                &mut worst_gain_ratio,
            );
            run_len = 0;
            run_min_gain = f32::MAX;
            run_max_gain = 0.0;
        }
    }
    close_window(
        run_len,
        run_min_gain,
        run_max_gain,
        &mut first_window,
        &mut worst_gain_ratio,
    );

    println!();
    println!("=== Summary ===");
    println!("Frames total    : {n_frames}");
    println!("Frames w/ speech: {speech_frames}");
    println!("Avg raw RMS     : {:.4}", raw_rms_sum / n_frames as f64);
    println!("Avg output RMS  : {:.4}", agc_rms_sum / n_frames as f64);
    println!("LSNR range      : {:.1} .. {:.1} dB", lsnr_min, lsnr_max);
    if speech_frames > 0 {
        let avg_speech_out = agc_rms_speech_sum / speech_frames as f64;
        let avg_gain = gain_speech_sum / speech_frames as f64;
        println!("Avg output RMS (speech): {avg_speech_out:.4}");
        println!("Avg gain (speech): {avg_gain:.2}× ({:.1} dB)", 20.0 * avg_gain.log10());
        println!("Speech output    : min {min_agc_rms:.4}, max {max_agc_rms:.4}");
        println!(
            "Gain range (speech): {min_gain_speech:.2}× .. {max_gain_speech:.2}× ({:.1} dB)",
            20.0 * (max_gain_speech / min_gain_speech.max(0.0001)).log10()
        );
        println!(
            "Worst speech-window gain ratio (< 2.0 required): {worst_gain_ratio:.2}"
        );
    } else {
        println!("No speech frames detected by the LSNR VAD!");
    }
    println!("=== end probe ===");

    // ---- NEW behavior checks ----
    assert!(
        speech_frames >= 5,
        "probe needs speech on the LSNR VAD to validate the AGC: {speech_frames} frames"
    );
    // Volume: quiet speech must reach an audible level (~ -28 dBFS).
    let avg_speech_out = agc_rms_speech_sum / speech_frames as f64;
    assert!(
        avg_speech_out >= 0.04,
        "avg output RMS during speech too low: {avg_speech_out:.4} (< 0.04)"
    );
    // Stability: within any contiguous speech window (after the attack ramp),
    // gain must not vary more than 2× (6 dB) — the rate limiter's job.
    assert!(
        worst_gain_ratio < 2.0,
        "gain varies more than 6 dB within a speech window: ratio {worst_gain_ratio:.2}"
    );
}

/// Record a finished contiguous-speech window (skipping the first — the
/// one-time attack ramp) and fold its gain ratio into the worst seen.
fn close_window(
    run_len: u32,
    run_min_gain: f32,
    run_max_gain: f32,
    first_window: &mut bool,
    worst_gain_ratio: &mut f32,
) {
    if run_len < 5 {
        return;
    }
    if *first_window {
        *first_window = false;
        return;
    }
    let ratio = run_max_gain / run_min_gain.max(0.0001);
    *worst_gain_ratio = (*worst_gain_ratio).max(ratio);
}
