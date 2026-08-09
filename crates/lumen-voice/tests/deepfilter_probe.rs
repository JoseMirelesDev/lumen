//! DeepFilterNet3 denoiser probe — primary (DeepFilterNet, full-band) vs the
//! RNNoise light fallback, comparing CPU, RAM, and noise reduction.
//!
//! Run with `cargo test -p lumen-voice --test deepfilter_probe -- --nocapture`
//! (also runs in CI via the `voice-probe` job) so the numbers are visible.
//!
//! Measures, headless + deterministic:
//!   - CPU: real-time factor (RTF) and ms / 20 ms frame for the send path in
//!     LIGHT mode (`NoiseSuppressor::new_light`, RNNoise+WebRTC NS) vs the
//!     primary DeepFilterNet tier (`NoiseSuppressor::new_neural`), plus alone.
//!   - RAM (Linux): resident-set-size (VmRSS) delta for the DeepFilterNet stage.
//!   - NOISE REDUCTION: dB attenuation of a stationary-noise probe in both
//!     tiers, plus a speech-in-noise check.
//!   - STREAMING LATENCY: the buffering delay the denoiser adds.

use std::time::Instant;

use lumen_voice::audio::{DeepFilterDenoiser, NoiseSuppressor};

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

/// Amplitude-modulated harmonic stack — reads as speech to the VAD/denoisers.
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

/// Linux resident-set size in bytes (VmRSS from /proc/self/status).
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
fn deepfilter_perf_probe() {
    let df_avail = DeepFilterDenoiser::new().is_some();
    let audio_s = 10.0;
    let n_frames = (audio_s * RATE as f64 / FRAME as f64) as usize;
    let frame = tone_frame(220.0, 0);

    // --- CPU: LIGHT tier ---
    let mut light = NoiseSuppressor::new_light();
    let tb = Instant::now();
    let mut lsink = 0i64;
    for _ in 0..n_frames {
        lsink += light.process(&frame).iter().map(|s| *s as i64).sum::<i64>();
    }
    let light_rtf = tb.elapsed().as_secs_f64() / audio_s;

    // --- CPU: HIGH tier (DeepFilterNet engaged) ---
    let mut high = NoiseSuppressor::new_neural();
    let t0 = Instant::now();
    let mut hsink = 0i64;
    for _ in 0..n_frames {
        hsink += high.process(&frame).iter().map(|s| *s as i64).sum::<i64>();
    }
    let high_rtf = t0.elapsed().as_secs_f64() / audio_s;

    // --- CPU: DeepFilterNet alone ---
    let mut g_rtf = f64::NAN;
    if df_avail {
        let mut g = DeepFilterDenoiser::new().unwrap();
        let t1 = Instant::now();
        let mut gsink = 0i64;
        for _ in 0..n_frames {
            gsink += g.process(&frame).0.iter().map(|s| *s as i64).sum::<i64>();
        }
        g_rtf = t1.elapsed().as_secs_f64() / audio_s;
        assert_ne!(gsink, 0, "DeepFilterNet produced only silence");
    }

    // --- RAM: VmRSS delta for the DeepFilterNet stage (Linux) ---
    let mut rss_delta = None;
    if resident_set_bytes().is_some() {
        let mut b = NoiseSuppressor::new_light();
        for _ in 0..20 {
            b.process(&frame);
        }
        let rss_base = resident_set_bytes();
        drop(b);
        let mut o = NoiseSuppressor::new_neural();
        for _ in 0..20 {
            o.process(&frame);
        }
        let rss_on = resident_set_bytes();
        rss_delta = rss_on.zip(rss_base).map(|(on, base)| on.saturating_sub(base));
    }

    // --- DeepFilterNet streaming latency ---
    let mut g_latency_ms = f64::NAN;
    if df_avail {
        let mut g = DeepFilterDenoiser::new().unwrap();
        let silence = vec![0i16; FRAME];
        let onset_frame = 100;
        let tone = tone_frame(220.0, 0);
        let mut in_onset: Option<usize> = None;
        let mut out_onset: Option<usize> = None;
        for i in 0..300 {
            let inp = if i >= onset_frame { &tone } else { &silence };
            let (out, _lsnr) = g.process(inp);
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

    // --- Noise reduction (dB) on a stationary-noise probe ---
    // Realistic room level (RMS 0.02 ≈ -34 dBFS): the near-end rescue only
    // fires on clear speech (>= -26 dBFS), so the noise-reduction numbers
    // measure the denoisers, not the rescue.
    let mut noise = xorshift_noise(FRAME * 40, 4);
    {
        // rms() here is RAW (i16 scale): normalize to 0.02 * 32768.
        let g = (0.02 * 32768.0) / rms(&noise);
        for s in noise.iter_mut() {
            *s = ((*s as f64) * g).round() as i16;
        }
    }
    let probe = &noise[FRAME * 20..FRAME * 21];
    let probe_rms = rms(probe);

    let mut off = NoiseSuppressor::new_light();
    for c in noise.chunks(FRAME).take(20) {
        off.process(c);
    }
    let light_db = db_reduction(probe_rms, rms(&off.process(probe)));

    let mut on_db = 0.0;
    if df_avail {
        let mut on = NoiseSuppressor::new_neural();
        for c in noise.chunks(FRAME).take(20) {
            on.process(c);
        }
        on_db = db_reduction(probe_rms, rms(&on.process(probe)));
    }

    // --- Speech-in-noise survives the high tier ---
    let mut speech_in_noise_out_rms = 0.0;
    let mut noise_only_out_rms = 0.0;
    if df_avail {
        let mut sn = NoiseSuppressor::new_neural();
        for i in 0..20 {
            let sp = speech_frame(i);
            let mut mix: Vec<i16> = sp
                .iter()
                .zip(noise[i * FRAME..(i + 1) * FRAME].iter())
                .map(|(s, n)| (*s as i64 + *n as i64) as i16)
                .collect();
            let out = sn.process(&mut mix);
            if i == 19 {
                speech_in_noise_out_rms = rms(&out);
            }
        }
        let mut nn = NoiseSuppressor::new_neural();
        for c in noise.chunks(FRAME).take(20) {
            nn.process(c);
        }
        noise_only_out_rms = rms(&nn.process(&noise[FRAME * 20..FRAME * 21]));
    }

    // --- Report ---
    println!();
    println!("=== DeepFilterNet PROBE — LIGHT vs HIGH ===");
    println!("DeepFilterNet available : {df_avail}");
    println!("audio benchmarked              : {audio_s:.0} s ({n_frames} frames)");
    println!();
    println!("-- CPU (RTF = wall s / audio s; 1.0 = real-time) --");
    println!("LIGHT (RNNoise+NS)     RTF : {light_rtf:.3}  ({:.1}% of a core)", light_rtf * 100.0);
    println!("HIGH  (DeepFilterNet) RTF : {high_rtf:.3}  ({:.1}% of a core)", high_rtf * 100.0);
    if df_avail {
        println!("DeepFilterNet ALONE   RTF : {g_rtf:.3}");
        println!("DeepFilterNet STREAMING LATENCY     : {g_latency_ms:.0} ms  (want <= 80)");
    }
    println!();
    println!("-- RAM (Linux VmRSS, warmed chain) --");
    match rss_delta {
        Some(b) => println!("DeepFilterNet MARGINAL RSS          : +{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        None => println!("DeepFilterNet MARGINAL RSS          : n/a (non-Linux host)"),
    }
    println!();
    println!("-- NOISE REDUCTION (stationary noise, dB) --");
    println!("LIGHT : {light_db:.1} dB");
    println!("HIGH  : {on_db:.1} dB");
    if df_avail {
        println!("SPEECH+NOISE out RMS : {speech_in_noise_out_rms:.4} (noise-only out: {noise_only_out_rms:.4})");
    }
    println!("=== end probe ===");
    println!();

    assert!(df_avail, "DeepFilterNet failed to load on this host");
    assert!(light_rtf < 1.0, "light tier cannot keep up with real-time: RTF {light_rtf:.3}");
    // Debug-build tract is ~5-10x slower than release (measured release RTF
    // 0.21 on this host with the full-DNN path). The gate is a rough
    // regression bound, not a release budget.
    assert!(high_rtf < 2.5, "high tier cannot keep up with real-time (debug build): RTF {high_rtf:.3}");
    if df_avail {
        // Debug-build tract is ~5-10x slower than release (release g_rtf
        // measured 0.216 on this host). Gate is a rough regression bound.
        assert!(g_rtf < 2.0, "DeepFilterNet alone too slow for real-time (debug build): RTF {g_rtf:.3}");
        assert!(g_latency_ms <= 80.0, "DeepFilterNet streaming latency too high: {g_latency_ms:.0} ms");
        assert!(on_db > light_db, "DeepFilterNet should reduce noise more than light: {light_db:.1} -> {on_db:.1} dB");
        assert!(on_db >= 40.0, "DeepFilterNet should deliver strong noise reduction: {on_db:.1} dB");
        assert!(speech_in_noise_out_rms > noise_only_out_rms, "speech must survive the high tier");
    }
    assert_ne!(lsink, 0, "light tier produced only silence");
    assert_ne!(hsink, 0, "high tier produced only silence");
}
