//! GTCRN (sherpa-onnx) denoiser probe — ON vs OFF comparison of CPU, RAM, and
//! noise reduction against the pre-GTCRN send path.
//!
//! Run with `cargo test -p lumen-voice --test gtcrn_probe -- --nocapture`
//! (also runs in CI via the `voice-probe` job) so the numbers are visible.
//!
//! Measures, headless + deterministic:
//!   - CPU: real-time factor (RTF) and ms / 20 ms frame for the FULL send path
//!     WITH GTCRN vs WITHOUT (the `new_without_neural_denoiser` baseline),
//!     plus GTCRN alone in isolation. Want RTF << 1.0; the on/off delta is the
//!     marginal neural cost.
//!   - RAM (Linux): resident-set-size (VmRSS) of a warmed chain WITHOUT vs
//!     WITH the GTCRN stage. The delta is the model + onnxruntime footprint.
//!   - NOISE REDUCTION: dB attenuation of the same deterministic stationary
//!     noise probe through the chain WITH vs WITHOUT GTCRN — the reduction
//!     delta — plus a speech-in-noise check that the denoiser suppresses the
//!     noise without killing the speech.
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

fn db_reduction(input_rms: f64, output_rms: f64) -> f64 {
    20.0 * (input_rms / output_rms.max(1e-9)).log10()
}

fn tone_frame(freq: f32, idx: usize) -> Vec<i16> {
    (0..FRAME)
        .map(|i| {
            let t = ((idx * FRAME + i) as f32) / RATE as f32;
            (0.3 * (std::f32::consts::TAU * freq * t).sin() * 32767.0) as i16
        })
        .collect()
}

/// Deterministic pseudo-random white noise (xorshift), moderate level.
fn xorshift_noise(samples: usize, scale: i16) -> Vec<i16> {
    let mut state = 0x1234_5678u32;
    let mut v = Vec::with_capacity(samples);
    for _ in 0..samples {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        v.push(((state >> 8) as i16) / scale);
    }
    v
}

/// Amplitude-modulated harmonic stack — reads as speech to the VAD/denoisers
/// (pauses + dynamics, unlike a bare continuous buzz which gets gated).
fn speech_frame(idx: usize) -> Vec<i16> {
    (0..FRAME)
        .map(|i| {
            let t = ((idx * FRAME + i) as f64) / RATE as f64;
            let mut v = 0.0;
            for (n, amp) in [(1, 1.0), (2, 0.5), (3, 0.33), (4, 0.25), (5, 0.2)] {
                v += amp * (2.0 * std::f64::consts::PI * 150.0 * n as f64 * t).sin();
            }
            let am = 0.7 + 0.3 * (2.0 * std::f64::consts::PI * 8.0 * t).sin();
            (v * am * 3000.0) as i16
        })
        .collect()
}

/// Linux resident-set size in bytes (VmRSS from /proc/self/status). `None` on
/// non-Linux platforms where the RAM comparison is skipped.
fn resident_set_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

#[test]
fn gtcrn_perf_probe() {
    let gtcrn_avail = GtcrnDenoiser::new().is_some();

    // --- Deterministic audio for throughput (compute-bound, content agnostic) --
    let audio_s = 20.0;
    let n_frames = (audio_s * RATE as f64 / FRAME as f64) as usize; // 1000
    let frame = tone_frame(220.0, 0);

    // --- CPU: baseline full chain WITHOUT GTCRN ---------------------------------
    let mut base = NoiseSuppressor::new_without_neural_denoiser();
    let tb = Instant::now();
    let mut bsink = 0i64;
    for _ in 0..n_frames {
        let out = base.process(&frame);
        bsink += out.iter().map(|s| *s as i64).sum::<i64>();
    }
    let base_el = tb.elapsed().as_secs_f64();
    let base_rtf = base_el / audio_s;
    let base_ms = base_el * 1000.0 / n_frames as f64;

    // --- CPU: full chain WITH GTCRN (what the live mic path actually runs) -----
    let mut ns = NoiseSuppressor::new();
    let t0 = Instant::now();
    let mut sink = 0i64;
    for _ in 0..n_frames {
        let out = ns.process(&frame);
        sink += out.iter().map(|s| *s as i64).sum::<i64>();
    }
    let full_el = t0.elapsed().as_secs_f64();
    let full_rtf = full_el / audio_s;
    let full_ms = full_el * 1000.0 / n_frames as f64;

    // --- CPU: GTCRN alone (isolate the new neural cost) -------------------------
    let mut g_rtf = f64::NAN;
    let mut g_ms = f64::NAN;
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
        g_ms = g_el * 1000.0 / n_frames as f64;
        assert_ne!(gsink, 0, "GTCRN produced only silence");
    }

    // --- CPU summary -------------------------------------------------------------
    let gtcrn_marginal_ms = full_ms - base_ms;
    let gtcrn_marginal_pct = gtcrn_marginal_ms / 20.0 * 100.0; // % of the 20 ms budget

    // --- RAM: VmRSS of a warmed chain WITHOUT vs WITH GTCRN (Linux only) --------
    let mut rss_delta = None;
    if resident_set_bytes().is_some() {
        // Baseline chain, warmed (forces all its allocations).
        let mut b = NoiseSuppressor::new_without_neural_denoiser();
        for _ in 0..20 {
            b.process(&frame);
        }
        let rss_base = resident_set_bytes();
        drop(b);
        // With GTCRN, warmed (forces onnxruntime session + inference allocs).
        let mut o = NoiseSuppressor::new();
        for _ in 0..20 {
            o.process(&frame);
        }
        let rss_on = resident_set_bytes();
        rss_delta = rss_on.zip(rss_base).map(|(on, base)| on.saturating_sub(base));
    }
    let rss_delta_mb = rss_delta.map(|b| b as f64 / (1024.0 * 1024.0));

    // --- GTCRN streaming latency (onset of a tone burst through silence) --------
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

    // --- Noise reduction: dB attenuation on a shared stationary-noise probe ----
    let noise = xorshift_noise(960 * 40, 4); // RMS ~0.03, like a room fan/AC
    let probe = &noise[960 * 20..960 * 21];
    let probe_rms = rms(probe);

    let mut off = NoiseSuppressor::new_without_neural_denoiser();
    for c in noise.chunks(960).take(20) {
        off.process(c);
    }
    let off_out = off.process(probe);
    let off_db = db_reduction(probe_rms, rms(&off_out));

    let mut on = NoiseSuppressor::new();
    for c in noise.chunks(960).take(20) {
        on.process(c);
    }
    let on_out = on.process(probe);
    let on_db = db_reduction(probe_rms, rms(&on_out));
    let reduction_delta_db = on_db - off_db;
    let noise_only_out_rms = rms(&on_out);

    // --- Noise reduction: speech-in-noise is preserved (not gated to silence) --
    let mut sn = NoiseSuppressor::new();
    let speech = speech_frame(0);
    let speech_rms = rms(&speech);
    // Noise scaled to +10 dB SNR against the speech level, mixed per frame.
    let noise_mix = xorshift_noise(960 * 40, 4);
    let mix_noise_rms = rms(&noise_mix[..FRAME]);
    let noise_gain = speech_rms / (mix_noise_rms * 10f64.sqrt()); // 10 dB SNR
    let mut speech_in_noise_out_rms = 0.0;
    for i in 0..20 {
        let sp = speech_frame(i);
        let mut mix = Vec::with_capacity(FRAME);
        for (j, s) in sp.iter().enumerate() {
            mix.push((*s as f64 + noise_mix[i * FRAME + j] as f64 * noise_gain) as i16);
        }
        let out = sn.process(&mix);
        if i == 19 {
            speech_in_noise_out_rms = rms(&out);
        }
    }

    // --- Report ------------------------------------------------------------------
    println!();
    println!("=== GTCRN (sherpa-onnx) PROBE — ON vs OFF ===");
    println!("GTCRN (sherpa-onnx) available : {gtcrn_avail}");
    println!("audio benchmarked              : {audio_s:.0} s ({n_frames} frames)");
    println!();
    println!("-- CPU (RTF = wall s / audio s; 1.0 = real-time) --");
    println!("FULL CHAIN  WITHOUT GTCRN RTF : {base_rtf:.3}  ({base_ms:.2} ms / 20 ms)");
    println!("FULL CHAIN  WITH    GTCRN RTF : {full_rtf:.3}  ({full_ms:.2} ms / 20 ms)");
    println!("GTCRN MARGINAL COST            : +{gtcrn_marginal_ms:.2} ms / frame  ({gtcrn_marginal_pct:.1}% of the 20 ms budget)");
    if gtcrn_avail {
        println!("GTCRN ALONE RTF                : {g_rtf:.3}  ({g_ms:.2} ms / 20 ms)");
        println!("GTCRN STREAMING LATENCY        : {g_latency_ms:.0} ms  (want <= 80)");
    }
    println!();
    println!("-- RAM (Linux VmRSS, warmed chain) --");
    match rss_delta_mb {
        Some(mb) => {
            println!("GTCRN MARGINAL RSS             : +{mb:.1} MB (model + onnxruntime)");
        }
        None => println!("GTCRN MARGINAL RSS             : n/a (non-Linux host)"),
    }
    println!();
    println!("-- NOISE REDUCTION (stationary noise, dB attenuation) --");
    println!("WITHOUT GTCRN  : {off_db:.1} dB");
    println!("WITH    GTCRN  : {on_db:.1} dB");
    println!("REDUCTION DELTA: +{reduction_delta_db:.1} dB  (GTCRN's contribution)");
    println!(
        "SPEECH+NOISE out RMS : {speech_in_noise_out_rms:.4} (noise-only out: {noise_only_out_rms:.4}) — speech survives"
    );
    println!("=== end probe ===");
    println!();

    // Performance gates: must keep up with real-time with headroom.
    assert!(gtcrn_avail, "sherpa-onnx/GTCRN failed to load on this host");
    assert!(base_rtf < 1.0, "baseline chain cannot keep up: RTF {base_rtf:.3}");
    assert!(full_rtf < 1.0, "full chain cannot keep up with real-time: RTF {full_rtf:.3}");
    assert!(g_rtf < 0.5, "GTCRN alone too slow for real-time: RTF {g_rtf:.3}");
    assert!(g_latency_ms <= 80.0, "GTCRN streaming latency too high: {g_latency_ms:.0} ms");
    // Noise-reduction gates: GTCRN must add meaningful attenuation over the
    // baseline and must not gate speech away.
    assert!(on_db > off_db, "GTCRN should reduce noise more than baseline: {off_db:.1} -> {on_db:.1} dB");
    assert!(on_db >= 20.0, "GTCRN should deliver strong noise reduction: {on_db:.1} dB");
    assert!(
        speech_in_noise_out_rms > noise_only_out_rms,
        "speech must survive the denoiser: noise-only {noise_only_out_rms:.4} vs speech+noise {speech_in_noise_out_rms:.4}"
    );
    if let Some(mb) = rss_delta_mb {
        assert!(mb > 0.0, "GTCRN should add resident memory, delta was {mb:.1} MB");
    }
    assert_ne!(sink, 0, "full chain produced only silence");
    assert_ne!(bsink, 0, "baseline chain produced only silence");
}
