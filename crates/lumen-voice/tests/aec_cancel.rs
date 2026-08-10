//! AEC3 functional probe (deterministic, no hardware): does feeding the
//! far-end render reference into AEC3 actually cancel a speaker echo, and
//! does it preserve the near-end voice?
//!
//! Far-end (playback): 440 Hz tone, 10 s @ 48 kHz. Near-end (capture):
//!   - cancellation run: the same tone delayed 5 ms at -6 dB (a crude
//!     speaker echo), NO speech — so the 440 Hz bin is unambiguous;
//!   - voice runs: `testdata/speech.wav` (looped) and a synthetic f0=120 Hz
//!     voiced-like harmonic stack, each + the same echo. (speech.wav is
//!     ~zero autocorrelation at every lag, so waveform correlation through
//!     this spectrally-aggressive chain is meaningless on it — the synthetic
//!     near-end is the waveform-correlation case, speech.wav is measured on
//!     its 20 ms RMS envelope.)
//! Two production NsOnly chains run in lockstep 10 ms chunks (render fed in
//! 480-sample chunks per the `process_render_frame` contract, one render
//! frame per capture half-frame):
//!   - ns_on:  render fed (the `aec_enabled=true` path);
//!   - ns_off: no render (the `aec_enabled=false` path).
//! After the first 1 s (AEC3 convergence), measure over the last 5 s:
//!   - cancellation: Goertzel amplitude at 440 Hz, AEC-on output vs AEC-off
//!     output (same chain, only AEC differs) — PASS if >= 15 dB lower;
//!   - voice: best-lag waveform correlation (synthetic near-end) and 20 ms
//!     RMS-envelope correlation (both near-ends), AEC-on vs AEC-off, plus
//!     each against a clean reference (speech-only, no render) — PASS if
//!     >= 0.9.
//!
//! This is a MEASUREMENT, not a gate: whichever way it lands, the numbers
//! are printed. The plan's contingency decides what they mean.
//!
//! Run: `cargo test -p lumen-voice --test aec_cancel -- --nocapture`

use lumen_voice::audio::{load_wav_pcm, NoiseSuppressor, SuppressorModel};

const RATE: f64 = 48_000.0;
const FRAME: usize = 960; // 20 ms @ 48 kHz
const SECS: usize = 10; // total audio
const TOTAL_SAMPLES: usize = RATE as usize * SECS;
const WARMUP_SAMPLES: usize = RATE as usize * 1; // AEC3 convergence
const MEASURE_SAMPLES: usize = RATE as usize * 5; // last 5 s
const TONE_HZ: f64 = 440.0;
const ECHO_DELAY: usize = 240; // 5 ms @ 48 kHz

/// 440 Hz far-end tone at -0.9 dBFS (loud speaker playback).
fn far_end_tone() -> Vec<i16> {
    (0..TOTAL_SAMPLES)
        .map(|n| {
            ((2.0 * std::f64::consts::PI * TONE_HZ * n as f64 / RATE).sin() * 0.9 * 32767.0).round()
                as i16
        })
        .collect()
}

/// The far-end tone as it reaches the mic: delayed 5 ms, -6 dB.
fn echo_only(far: &[i16]) -> Vec<i16> {
    let mut out = vec![0i16; TOTAL_SAMPLES];
    for n in ECHO_DELAY..TOTAL_SAMPLES {
        out[n] = ((far[n - ECHO_DELAY] as i32 * 5) / 10).clamp(-32768, 32767) as i16;
    }
    out
}

/// speech.wav looped to 10 s.
fn speech_only() -> Vec<i16> {
    let speech0 = load_wav_pcm(&format!("{}/testdata/speech.wav", env!("CARGO_MANIFEST_DIR")))
        .expect("testdata/speech.wav not found — run from crates/lumen-voice/");
    let mut speech = Vec::with_capacity(TOTAL_SAMPLES);
    while speech.len() < TOTAL_SAMPLES {
        speech.extend_from_slice(&speech0);
    }
    speech.truncate(TOTAL_SAMPLES);
    speech
}

/// speech.wav (looped to 10 s) + the delayed -6 dB echo.
fn speech_plus_echo(far: &[i16]) -> Vec<i16> {
    let speech = speech_only();
    (0..TOTAL_SAMPLES)
        .map(|n| {
            let mut v = speech[n] as i32;
            if n >= ECHO_DELAY {
                v += far[n - ECHO_DELAY] as i32 * 5 / 10;
            }
            v.clamp(-32768, 32767) as i16
        })
        .collect()
}

/// RBJ biquad notch at 440 Hz — removes the echo tone before correlating,
/// so the voice comparison is not dominated by the tone itself.
fn notch_coeffs(f0: f64, q: f64) -> [f64; 5] {
    let w0 = 2.0 * std::f64::consts::PI * f0 / RATE;
    let alpha = w0.sin() / (2.0 * q);
    let cw = w0.cos();
    let (b0, b1, b2) = (1.0, -2.0 * cw, 1.0);
    let (a0, a1, a2) = (1.0 + alpha, -2.0 * cw, 1.0 - alpha);
    [b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0]
}

fn biquad(xs: &[i16], c: &[f64; 5]) -> Vec<f64> {
    let (mut x1, mut x2, mut y1, mut y2) = (0.0, 0.0, 0.0, 0.0);
    let mut out = Vec::with_capacity(xs.len());
    for &x in xs {
        let x0 = x as f64 / 32768.0;
        let y0 = c[0] * x0 + c[1] * x1 + c[2] * x2 - c[3] * y1 - c[4] * y2;
        x2 = x1;
        x1 = x0;
        y2 = y1;
        y1 = y0;
        out.push(y0);
    }
    out
}

/// Amplitude of the `freq` component over the whole window (Goertzel).
fn goertzel_amp(xs: &[i16], freq: f64) -> f64 {
    let n = xs.len() as f64;
    let w = 2.0 * std::f64::consts::PI * freq / RATE;
    let coeff = 2.0 * w.cos();
    let (mut s0, mut s1, mut s2) = (0.0, 0.0, 0.0);
    for &x in xs {
        s0 = (x as f64 / 32768.0) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let _ = s0; // final s0 is only ever copied into s1 (Goertzel recurrence)
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    (2.0 * power.sqrt() / n).max(1e-12)
}

fn rms_f64(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    (xs.iter().map(|x| x * x).sum::<f64>() / xs.len() as f64).sqrt()
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        sa += x;
        sb += y;
        saa += x * x;
        sbb += y * y;
        sab += x * y;
    }
    let cov = sab - sa * sb / n;
    let va = saa - sa * sa / n;
    let vb = sbb - sb * sb / n;
    cov / (va * vb).sqrt().max(1e-12)
}

/// Per-20 ms RMS normalization (removes slowly-varying GC2 gain divergence),
/// then Pearson over the whole window.
fn pearson_norm_frames(a: &[f64], b: &[f64]) -> f64 {
    let mut aa = Vec::with_capacity(a.len());
    let mut bb = Vec::with_capacity(b.len());
    for (fa, fb) in a.chunks_exact(FRAME).zip(b.chunks_exact(FRAME)) {
        let ra = rms_f64(fa);
        let rb = rms_f64(fb);
        let ga = if ra > 1e-6 { 1.0 / ra } else { 0.0 };
        let gb = if rb > 1e-6 { 1.0 / rb } else { 0.0 };
        aa.extend(fa.iter().map(|&x| x * ga));
        bb.extend(fb.iter().map(|&x| x * gb));
    }
    pearson(&aa, &bb)
}

/// Voiced-like near-end: f0 = 120 Hz harmonic stack (strong temporal
/// structure, unlike testdata/speech.wav which is ~0 autocorrelation at every
/// lag) + the -6 dB delayed echo. Lets waveform correlation measure voice
/// preservation through the chain.
fn synthetic_voice_plus_echo(far: &[i16]) -> Vec<i16> {
    let mut out = Vec::with_capacity(TOTAL_SAMPLES);
    for n in 0..TOTAL_SAMPLES {
        let t = n as f64 / RATE;
        let am = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * 4.0 * t).sin(); // 4 Hz amplitude mod
        let mut v = 0.0;
        for (k, a) in [(1usize, 0.5f64), (2, 0.35), (3, 0.22), (4, 0.14), (5, 0.09)] {
            v += a * (2.0 * std::f64::consts::PI * 120.0 * k as f64 * t).sin();
        }
        v *= am * 0.28;
        let mut s = (v * 32767.0).round() as i32;
        if n >= ECHO_DELAY {
            s += far[n - ECHO_DELAY] as i32 * 5 / 10;
        }
        out.push(s.clamp(-32768, 32767) as i16);
    }
    out
}

/// Per-20 ms RMS envelope (temporal dynamics), robust to spectral coloring.
fn envelope(xs: &[i16]) -> Vec<f64> {
    xs.chunks_exact(FRAME)
        .map(|c| rms_f64(&c.iter().map(|&x| x as f64).collect::<Vec<_>>()))
        .collect()
}

/// Pearson at the lag (in samples) maximizing correlation, over ±`max_lag`.
fn best_lag_corr(a: &[f64], b: &[f64], max_lag: usize) -> (usize, f64) {
    let mut best = (0usize, -1.0f64);
    let step = 240; // 5 ms
    let mut lag = 0;
    while lag <= max_lag {
        let c = pearson(&a[lag..], &b[..b.len() - lag]);
        if c > best.1 {
            best = (lag, c);
        }
        lag += step;
    }
    // Refine ±5 ms around the coarse best.
    let lo = best.0.saturating_sub(step);
    let hi = (best.0 + step).min(max_lag);
    let mut lag = lo;
    while lag <= hi {
        let c = pearson(&a[lag..], &b[..b.len() - lag]);
        if c > best.1 {
            best = (lag, c);
        }
        lag += 48; // 1 ms
    }
    best
}

/// Per-second 440 Hz suppression trend (echo-only capture): is any
/// cancellation slow convergence or absent entirely?
fn suppression_trend(capture: &[i16], far: &[i16], feed_render: bool) -> Vec<f64> {
    let mut ns = NoiseSuppressor::with_model(SuppressorModel::NsOnly);
    let mut out = Vec::new();
    let mut acc_in = Vec::with_capacity(RATE as usize);
    let mut acc_out = Vec::with_capacity(RATE as usize);
    for (i, chunk) in capture.chunks_exact(FRAME).enumerate() {
        let start = i * FRAME;
        if feed_render {
            for render_chunk in far[start..start + FRAME].chunks_exact(480) {
                ns.process_render_frame(render_chunk);
            }
        }
        let o = ns.process_gated(chunk).expect("process_gated always returns Some");
        acc_in.extend_from_slice(chunk);
        acc_out.extend_from_slice(&o);
        if acc_in.len() >= RATE as usize {
            out.push(20.0 * (goertzel_amp(&acc_out, TONE_HZ) / goertzel_amp(&acc_in, TONE_HZ)).log10());
            acc_in.clear();
            acc_out.clear();
        }
    }
    out
}

/// Run the production NsOnly chain over a capture, optionally feeding the
/// render reference. Returns (measurement-window input, output).
fn run_chain(capture: &[i16], far: &[i16], feed_render: bool) -> (Vec<i16>, Vec<i16>) {
    let mut ns = NoiseSuppressor::with_model(SuppressorModel::NsOnly);
    let mut win_in = Vec::new();
    let mut win_out = Vec::new();
    for (i, chunk) in capture.chunks_exact(FRAME).enumerate() {
        let start = i * FRAME;
        if feed_render {
            // One render frame per capture half-frame (10 ms chunks) — the
            // same shape as the production send task.
            for render_chunk in far[start..start + FRAME].chunks_exact(480) {
                ns.process_render_frame(render_chunk);
            }
        }
        let out = ns.process_gated(chunk).expect("process_gated always returns Some");
        // Last 5 s only (t = 5-10 s) — past the 1 s AEC3 convergence window.
        if start >= TOTAL_SAMPLES - MEASURE_SAMPLES {
            win_in.extend_from_slice(chunk);
            win_out.extend_from_slice(&out);
        }
    }
    assert_eq!(win_in.len(), MEASURE_SAMPLES);
    assert_eq!(win_out.len(), MEASURE_SAMPLES);
    (win_in, win_out)
}

#[test]
fn aec_cancel_probe() {
    let far = far_end_tone();

    // --- Cancellation: echo-only capture (no speech) -----------------------
    let echo = echo_only(&far);
    let (e_in, e_on) = run_chain(&echo, &far, true);
    let (_, e_off) = run_chain(&echo, &far, false);

    let a_in = goertzel_amp(&e_in, TONE_HZ);
    let a_on = goertzel_amp(&e_on, TONE_HZ);
    let a_off = goertzel_amp(&e_off, TONE_HZ);
    let s_full_on = 20.0 * (a_on / a_in).log10(); // AEC-on path vs raw input
    let s_full_off = 20.0 * (a_off / a_in).log10(); // AEC-off path vs raw input
    let s_aec = 20.0 * (a_on / a_off).log10(); // AEC's own contribution

    // --- Voice preservation: speech + echo capture -------------------------
    let sp_echo = speech_plus_echo(&far);
    let (_, v_on) = run_chain(&sp_echo, &far, true);
    let (_, v_off) = run_chain(&sp_echo, &far, false);

    // Notch out the echo tone so the correlation measures the voice, not the
    // tone that only one of the two paths cancels.
    let notch = notch_coeffs(TONE_HZ, 30.0);
    let on_n = biquad(&v_on, &notch);
    let off_n = biquad(&v_off, &notch);
    let corr_norm = pearson_norm_frames(&on_n, &off_n);
    let raw_on: Vec<f64> = v_on.iter().map(|&x| x as f64).collect();
    let raw_off: Vec<f64> = v_off.iter().map(|&x| x as f64).collect();
    let corr_raw = pearson(&raw_on, &raw_off);

    // Failure-mode attribution: compare each path against the clean reference
    // (speech-only capture, no render). If the no-render path tracks the
    // reference and the render-fed path does not, the render feed itself
    // corrupts the send (the historical finding); if neither does, the chain
    // damages the voice regardless of render.
    let speech = speech_only();
    let (_, ref_out) = run_chain(&speech, &far, false);
    let ref_n = biquad(&ref_out, &notch);
    let corr_on_ref = pearson_norm_frames(&on_n, &ref_n);
    let corr_off_ref = pearson_norm_frames(&off_n, &ref_n);
    // Chain fidelity on clean speech (no echo, no render): isolates whether
    // the 0.627 above is an echo/GC2 confound or a chain defect.
    let speech_n = biquad(&speech[..MEASURE_SAMPLES], &notch);
    let corr_chain_clean = pearson_norm_frames(&ref_n, &speech_n);
    // The APM has algorithmic delay, so lag-0 Pearson underestimates voice
    // preservation — sweep lags to find the aligned correlation (clean
    // speech in -> no-render chain out).
    let speech_f64: Vec<f64> = speech[..MEASURE_SAMPLES].iter().map(|&x| x as f64).collect();
    let ref_f64: Vec<f64> = ref_out.iter().map(|&x| x as f64).collect();
    let lag_sweep: Vec<(usize, f64)> = [0usize, 480, 960, 1440, 1920, 2880, 3840]
        .iter()
        .map(|&lag| (lag, pearson(&speech_f64[lag..], &ref_f64[..MEASURE_SAMPLES - lag])))
        .collect();

    // Convergence trend: per-second 440 Hz suppression on the echo-only run.
    let trend_on = suppression_trend(&echo, &far, true);
    let trend_off = suppression_trend(&echo, &far, false);

    // Structured near-end (strong autocorrelation): does the render feed
    // corrupt the waveform, or was the speech.wav correlation just a
    // structureless-sample artifact? Waveform corr at best lag + envelope corr.
    let syn = synthetic_voice_plus_echo(&far);
    let (_, s_on) = run_chain(&syn, &far, true);
    let (_, s_off) = run_chain(&syn, &far, false);
    let s_on_f: Vec<f64> = s_on.iter().map(|&x| x as f64).collect();
    let s_off_f: Vec<f64> = s_off.iter().map(|&x| x as f64).collect();
    let (_, syn_wave) = best_lag_corr(&s_on_f, &s_off_f, 3840);
    let syn_env_on = envelope(&s_on);
    let syn_env_off = envelope(&s_off);
    let (_, syn_env) = best_lag_corr(&syn_env_on, &syn_env_off, 6);

    // Envelope correlation for the speech case (temporal dynamics preserved?).
    let env_on = envelope(&v_on);
    let env_off = envelope(&v_off);
    let (_, env_on_off) = best_lag_corr(&env_on, &env_off, 6);
    let env_ref = envelope(&ref_out);
    let (_, env_off_ref) = best_lag_corr(&env_off, &env_ref, 6);
    let (_, env_on_ref) = best_lag_corr(&env_on, &env_ref, 6);

    // Harness sanity: two identical no-render runs must be bit-identical.
    let (_, d1) = run_chain(&sp_echo, &far, false);
    let (_, d2) = run_chain(&sp_echo, &far, false);
    let det_corr = pearson(
        &d1.iter().map(|&x| x as f64).collect::<Vec<_>>(),
        &d2.iter().map(|&x| x as f64).collect::<Vec<_>>(),
    );

    fn dbfs(xs: &[i16]) -> f64 {
        let r = rms_f64(&xs.iter().map(|&x| x as f64).collect::<Vec<_>>());
        if r > 1e-6 { 20.0 * (r / 32768.0).log10() } else { -120.0 }
    }

    println!("=== AEC3 functional probe (10 s @ 48 kHz, NsOnly chain) ===");
    println!("far-end: {TONE_HZ} Hz tone; echo in capture: -6 dB, +5 ms");
    println!(
        "near-end voice: testdata/speech.wav (looped); window: t = 5-10 s (after {} s convergence)",
        WARMUP_SAMPLES / RATE as usize
    );
    println!();
    println!("[cancellation] echo-only capture, 440 Hz Goertzel amplitude (dB re input):");
    println!("  AEC-off (no render):  {s_full_off:6.1} dB");
    println!("  AEC-on  (render fed): {s_full_on:6.1} dB");
    println!("  AEC-attributed (on vs off): {s_aec:6.1} dB   <- PASS if <= -15");
    println!();
    println!("[voice preservation] speech + echo capture, AEC-on vs AEC-off outputs:");
    println!("  correlation (rms-norm, 440 Hz notch): {corr_norm:.3}  <- PASS if >= 0.9");
    println!("  correlation (raw full-band):          {corr_raw:.3}");
    println!("  vs clean reference (speech-only, no render), rms-norm notch:");
    println!("    AEC-off (no render):  {corr_off_ref:.3}");
    println!("    AEC-on  (render fed): {corr_on_ref:.3}");
    println!("  chain fidelity, clean speech in -> out (no echo/render): {corr_chain_clean:.3}");
    println!(
        "  lag sweep (in vs no-render out), corr @ 0/10/20/30/40/60/80 ms: {}",
        lag_sweep
            .iter()
            .map(|(l, c)| format!("{l}:{c:.3}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    println!();
    println!("[voice, structured near-end] synthetic f0=120 Hz voice + echo, on vs off:");
    println!("  waveform corr @ best lag: {syn_wave:.3}   envelope corr: {syn_env:.3}");
    println!(
        "[voice, speech.wav] 20 ms envelope corr (temporal dynamics): on vs off {env_on_off:.3} | off vs ref {env_off_ref:.3} | on vs ref {env_on_ref:.3}"
    );
    println!();
    println!("[trend] per-second 440 Hz suppression (dB re input), echo-only capture:");
    println!("  sec :  1     2     3     4     5     6     7     8     9    10");
    println!(
        "  off : {}",
        trend_off.iter().map(|v| format!("{v:5.1}")).collect::<Vec<_>>().join(" ")
    );
    println!(
        "  on  : {}",
        trend_on.iter().map(|v| format!("{v:5.1}")).collect::<Vec<_>>().join(" ")
    );
    println!();
    println!(
        "[levels] RMS dBFS (in / AEC-off out / AEC-on out)  echo-only: {:.0} / {:.0} / {:.0}   speech: {:.0} / {:.0} / {:.0}",
        dbfs(&e_in), dbfs(&e_off), dbfs(&e_on),
        dbfs(&sp_echo[..MEASURE_SAMPLES.min(sp_echo.len())]), dbfs(&v_off), dbfs(&v_on),
    );
    println!("[sanity] identical no-render runs correlate: {det_corr:.3} (expect 1.000)");
    println!();
    println!(
        "==> echo cancelled >= 15 dB (AEC-attributed): {}",
        if s_aec <= -15.0 { "YES" } else { "no" }
    );
    println!(
        "==> voice preserved (best-lag waveform >= 0.9): {}",
        if syn_wave >= 0.9 { "YES" } else { "no" }
    );
    println!("note: measurement, not a gate — the plan contingency interprets these numbers.");
    println!(
        "note: the tone suppression on BOTH paths comes from WebRTC NS, not AEC3 (-22 dB off-path);"
    );
    println!(
        "note: a pure-tone far-end is a worst case for AEC3 linear-filter stability (see the trend),"
    );
    println!(
        "note: and a voice echo (non-stationary) is not modeled here — the render path stays a separate investigation."
    );
}
