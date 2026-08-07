//! GTCRN performance probe — does the sherpa-onnx neural denoiser keep up with
//! real-time audio on this host, and how much latency does it add? This is a
//! PERFORMANCE probe (not quality): the thing that matters for live voice is
//! that each 20 ms capture frame is processed in well under 20 ms (real-time
//! factor < 1.0, ideally with headroom) and that the streaming denoiser
//! doesn't add objectionable delay.
//!
//! Run with `cargo test -p lumen-voice --test gtcrn_probe -- --nocapture`
//! (also runs in CI via the `voice-probe` job) so the numbers are visible.
//!
//! Measures, headless + deterministic:
//!   - FULL-CHAIN RTF: wall time to run `NoiseSuppressor` (WebRTC AEC3+NS,
//!     RNNoise, GTCRN, leveler) over N seconds of audio / N. Want << 1.0.
//!   - GTCRN-ALONE RTF: the neural stage in isolation (the new cost).
//!   - ms/frame: wall time per 20 ms capture frame.
//!   - GTCRN STREAMING LATENCY: the buffering delay the online denoiser adds
//!     (measured by onset detection of a tone burst through silence).

use std::time::Instant;

use lumen_voice::audio::{GtcrnDenoiser, NoiseSuppressor};

const RATE: u32 = 48_000;
const FRAME: usize = 960; // 20 ms @ 48 kHz

fn rms(v: &[i16]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let s: f64 = v.iter().map(|x| (*x as f64) * (*x as f64)).sum();
    (s / v.len() as f64).sqrt()
}

fn tone_frame(freq: f32, idx: usize) -> Vec<i16> {
    (0..FRAME)
        .map(|i| {
            let t = ((idx * FRAME + i) as f32) / RATE as f32;
            (0.3 * (std::f32::consts::TAU * freq * t).sin() * 32767.0) as i16
        })
        .collect()
}

#[test]
fn gtcrn_perf_probe() {
    let gtcrn_avail = GtcrnDenoiser::new().is_some();

    // --- Deterministic audio for throughput (compute-bound, content agnostic) --
    let audio_s = 20.0;
    let n_frames = (audio_s * RATE as f64 / FRAME as f64) as usize; // 1000
    let frame = tone_frame(220.0, 0);

    // --- Full chain throughput (what the live mic path actually runs) ---------
    let mut ns = NoiseSuppressor::new();
    let t0 = Instant::now();
    let mut sink = 0i64;
    for i in 0..n_frames {
        let f = if i % 2 == 0 { &frame } else { &frame };
        let out = ns.process(f);
        sink += out.iter().map(|s| *s as i64).sum::<i64>();
    }
    let full_el = t0.elapsed().as_secs_f64();
    let full_rtf = full_el / audio_s;
    let full_ms_per_frame = full_el * 1000.0 / n_frames as f64;

    // --- GTCRN alone (isolate the new neural cost) -----------------------------
    let mut g_rtf = f64::NAN;
    let mut g_ms_per_frame = f64::NAN;
    if gtcrn_avail {
        let mut g = GtcrnDenoiser::new().unwrap();
        let t1 = Instant::now();
        let mut gsink = 0i64;
        for _ in 0..n_frames {
            let out = g.process(&frame);
            gsink += out.iter().map(|s| *s as i64).sum::<i64>();
        }
        let g_el = t1.elapsed().as_secs_f64();
        g_rtf = g_el / audio_s;
        g_ms_per_frame = g_el * 1000.0 / n_frames as f64;
        assert_ne!(gsink, 0, "GTCRN produced only silence");
    }

    // --- GTCRN streaming latency (onset of a tone burst through silence) -------
    let mut g_latency_ms = f64::NAN;
    if gtcrn_avail {
        let mut g = GtcrnDenoiser::new().unwrap();
        let silence = vec![0i16; FRAME];
        let onset_frame = 100; // 2 s of silence, then a tone burst
        let tone = tone_frame(220.0, 0);
        let mut in_onset: Option<usize> = None;
        let mut out_onset: Option<usize> = None;
        for i in 0..300 {
            let inp = if i >= onset_frame { &tone } else { &silence };
            let out = g.process(inp);
            if i >= onset_frame && in_onset.is_none() {
                in_onset = Some(i);
            }
            if out_onset.is_none() && rms(&out) > 0.005 {
                out_onset = Some(i);
            }
        }
        if let (Some(i), Some(o)) = (in_onset, out_onset) {
            g_latency_ms = (o.saturating_sub(i)) as f64 * 20.0;
        }
    }

    // --- Report ------------------------------------------------------------------
    println!();
    println!("=== GTCRN PERFORMANCE PROBE ===");
    println!("GTCRN (sherpa-onnx) available : {gtcrn_avail}");
    println!("audio benchmarked              : {audio_s:.0} s ({n_frames} frames)");
    println!("FULL CHAIN  RTF  : {full_rtf:.3}  ({full_ms_per_frame:.2} ms / 20 ms frame)");
    if gtcrn_avail {
        println!("GTCRN ALONE RTF  : {g_rtf:.3}  ({g_ms_per_frame:.2} ms / 20 ms frame)");
        println!("GTCRN STREAMING LATENCY : {g_latency_ms:.0} ms  (want <= 80)");
    }
    println!("=== end probe ===");
    println!();

    // Performance gates: must keep up with real-time with headroom.
    assert!(gtcrn_avail, "sherpa-onnx/GTCRN failed to load on this host");
    assert!(full_rtf < 1.0, "full chain cannot keep up with real-time: RTF {full_rtf:.3}");
    assert!(g_rtf < 0.5, "GTCRN alone too slow for real-time: RTF {g_rtf:.3}");
    assert!(g_latency_ms <= 80.0, "GTCRN streaming latency too high: {g_latency_ms:.0} ms");
    assert_ne!(sink, 0, "full chain produced only silence");
}
