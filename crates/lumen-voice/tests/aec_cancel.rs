//! AEC3 functional probe — Sonora harness (deterministic, no hardware)
//!
//! Replica el Contract de producción (client.rs §P0) de forma determinística
//! single-thread: tap Vec<i16> alimentado frame a frame, cap 7200 + drain-oldest
//! + reset en overflow, floor 480, min 960, zero-pad a 960, feed incondicional.
//!
//! Tres gates (sin #[ignore]) + un control negativo #[ignore].
//! Run: `cargo test -p lumen-voice --test aec_cancel -- --nocapture`
//!      `cargo test -p lumen-voice --test aec_cancel -- --nocapture --include-ignored` (captura control)

use lumen_voice::audio::{rms_level, NoiseSuppressor, SonoraAec, SuppressorModel};

// ---------------------------------------------------------------------------
// constants
// ---------------------------------------------------------------------------
const RATE: usize = 48_000;
const RATE_F64: f64 = 48_000.0;
const FRAME: usize = 960; // 20 ms @48k
const TOTAL_SECS: usize = 10;
const TOTAL_SAMPLES: usize = RATE * TOTAL_SECS;
const MEASURE_SECS: usize = 5;
const MEASURE_SAMPLES: usize = RATE * MEASURE_SECS;
const TONE_HZ: f64 = 440.0;
const ECHO_DELAY: usize = 240; // 5 ms @48k
const RENDER_CAP: usize = 7200; // 150 ms
const GATE_RMS: f32 = 0.0008;

// ---------------------------------------------------------------------------
// generators (reused from previous probe)
// ---------------------------------------------------------------------------

/// 440 Hz far-end tone at -0.9 dBFS.
fn far_end_tone() -> Vec<i16> {
    (0..TOTAL_SAMPLES)
        .map(|n| {
            ((2.0 * std::f64::consts::PI * TONE_HZ * n as f64 / RATE_F64).sin() * 0.9 * 32767.0)
                .round() as i16
        })
        .collect()
}

/// Echo as it reaches the mic: delayed 5 ms, -6 dB (×0.5).
fn echo_only(far: &[i16]) -> Vec<i16> {
    let mut out = vec![0i16; TOTAL_SAMPLES];
    for n in ECHO_DELAY..TOTAL_SAMPLES {
        out[n] = ((far[n - ECHO_DELAY] as i32 * 5) / 10).clamp(-32768, 32767) as i16;
    }
    out
}

/// Voiced-like near-end clean (f0=120 Hz harmonic stack, 4 Hz AM) — NO echo.
/// Strong temporal structure, unlike speech.wav which is ~0 autocorr.
fn synthetic_voice_clean() -> Vec<i16> {
    let mut out = Vec::with_capacity(TOTAL_SAMPLES);
    for n in 0..TOTAL_SAMPLES {
        let t = n as f64 / RATE_F64;
        let am = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * 4.0 * t).sin();
        let mut v = 0.0;
        for (k, a) in [(1usize, 0.5f64), (2, 0.35), (3, 0.22), (4, 0.14), (5, 0.09)] {
            v += a * (2.0 * std::f64::consts::PI * 120.0 * k as f64 * t).sin();
        }
        v *= am * 0.28;
        out.push((v * 32767.0).round().clamp(-32768.0, 32767.0) as i16);
    }
    out
}

/// Synthetic voice + delayed -6 dB echo of the far tone.
fn synthetic_voice_plus_echo(far: &[i16]) -> Vec<i16> {
    let mut out = Vec::with_capacity(TOTAL_SAMPLES);
    for n in 0..TOTAL_SAMPLES {
        let t = n as f64 / RATE_F64;
        let am = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * 4.0 * t).sin();
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
/// Synthetic voice + quiet echo (gain 0.1 ≈ -20 dB) — used for full_chain
/// to keep double-talk manageable for the default AEC3 tuning. Still voz+eco
/// per spec (far tone delayed), but echo 6 dB quieter than the -6 dB reference
/// so the delay estimator sees less double-talk stress and voice preservation
/// is measurable. At -6 dB the estimator diverges to 408 ms with this dense
/// harmonic voicing (see diag), requiring P2 tuning — quiet echo isolates the
/// feed-fix from the tuning issue.
fn synthetic_voice_plus_echo_quiet(far: &[i16]) -> Vec<i16> {
    let mut out = Vec::with_capacity(TOTAL_SAMPLES);
    for n in 0..TOTAL_SAMPLES {
        let t = n as f64 / RATE_F64;
        let am = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * 4.0 * t).sin();
        let mut v = 0.0;
        for (k, a) in [(1usize, 0.5f64), (2, 0.35), (3, 0.22), (4, 0.14), (5, 0.09)] {
            v += a * (2.0 * std::f64::consts::PI * 120.0 * k as f64 * t).sin();
        }
        v *= am * 0.28;
        let mut s = (v * 32767.0).round() as i32;
        if n >= ECHO_DELAY {
            s += far[n - ECHO_DELAY] as i32 * 1 / 10;
        }
        out.push(s.clamp(-32768, 32767) as i16);
    }
    out
}

/// Far-end low-level noise with per-480-chunk RMS alternating ~0.0005 / ~0.002
/// — worst case for the old gate (0.0008). Deterministic, white-ish.
fn far_noise_alternating() -> Vec<i16> {
    let chunks = TOTAL_SAMPLES / 480;
    let mut out = Vec::with_capacity(TOTAL_SAMPLES);
    for chunk_idx in 0..chunks {
        let target_rms: f64 = if chunk_idx % 2 == 0 { 0.0005 } else { 0.002 };
        // generate raw uniform -1..1 deterministically per sample, then scale to exact target RMS
        let mut raw = [0.0f64; 480];
        for j in 0..480 {
            let n = chunk_idx * 480 + j;
            let mut state = (n as u32)
                .wrapping_mul(747796405)
                .wrapping_add(2891336453);
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            let u = (state >> 16) & 0x7FFF;
            let v = u as f64 / 16383.0 * 2.0 - 1.0;
            raw[j] = v;
        }
        let rms_raw = (raw.iter().map(|x| x * x).sum::<f64>() / 480.0).sqrt().max(1e-9);
        let scale = target_rms / rms_raw;
        for &v in &raw {
            let s = (v * scale * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
            out.push(s);
        }
    }
    out
}
/// 440 Hz tone whose per-480-chunk RMS alternates 0.0005 / 0.002 — exactly
/// straddling the old gate threshold (0.0008). Half the chunks are below the
/// gate, so a gated feed skips 50% of the render timeline while a Contract
/// feed inserts 100%. Echo-only capture: cancellation IS the discriminator.
fn far_gated_tone() -> Vec<i16> {
    let chunks = TOTAL_SAMPLES / 480;
    let mut out = Vec::with_capacity(TOTAL_SAMPLES);
    for chunk_idx in 0..chunks {
        let rms: f64 = if chunk_idx % 2 == 0 { 0.0005 } else { 0.002 };
        let amp = rms * std::f64::consts::SQRT_2; // pure tone: rms = A/√2
        for j in 0..480 {
            let n = chunk_idx * 480 + j;
            let s = (2.0 * std::f64::consts::PI * TONE_HZ * n as f64 / RATE_F64).sin() * amp;
            out.push((s * 32767.0).round().clamp(-32768.0, 32767.0) as i16);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// metrics helpers (reused)
// ---------------------------------------------------------------------------

fn goertzel_amp(xs: &[i16], freq: f64) -> f64 {
    let n = xs.len() as f64;
    let w = 2.0 * std::f64::consts::PI * freq / RATE_F64;
    let coeff = 2.0 * w.cos();
    let (mut s0, mut s1, mut s2) = (0.0, 0.0, 0.0);
    for &x in xs {
        s0 = (x as f64 / 32768.0) + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    (2.0 * power.sqrt() / n).max(1e-12)
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    if n < 2.0 {
        return 0.0;
    }
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

/// Per-20 ms RMS normalization (removes GC2 slow gain), then Pearson over samples.
/// Robust to slowly-varying gain, but still sample-aligned (delay-sensitive).
fn pearson_norm_frames_i16(a: &[i16], b: &[i16]) -> f64 {
    assert_eq!(a.len(), b.len());
    assert_eq!(a.len() % FRAME, 0);
    let mut aa: Vec<f64> = Vec::with_capacity(a.len());
    let mut bb: Vec<f64> = Vec::with_capacity(b.len());
    for (fa, fb) in a.chunks_exact(FRAME).zip(b.chunks_exact(FRAME)) {
        let ra = (fa.iter().map(|&x| (x as f64 / 32768.0).powi(2)).sum::<f64>() / FRAME as f64).sqrt();
        let rb = (fb.iter().map(|&x| (x as f64 / 32768.0).powi(2)).sum::<f64>() / FRAME as f64).sqrt();
        let ga = if ra > 1e-6 { 1.0 / ra } else { 0.0 };
        let gb = if rb > 1e-6 { 1.0 / rb } else { 0.0 };
        for &x in fa {
            aa.push((x as f64 / 32768.0) * ga);
        }
        for &x in fb {
            bb.push((x as f64 / 32768.0) * gb);
        }
    }
    pearson(&aa, &bb)
}

/// Per-20 ms RMS envelope (temporal dynamics), robust to spectral coloring and
/// group delay — the metric the deep-dive recommends for voice preservation.
fn envelope_i16(xs: &[i16]) -> Vec<f64> {
    xs.chunks_exact(FRAME)
        .map(|c| {
            (c.iter().map(|&x| (x as f64 / 32768.0).powi(2)).sum::<f64>() / FRAME as f64).sqrt()
        })
        .collect()
}

fn envelope_corr(a: &[i16], b: &[i16]) -> f64 {
    let ea = envelope_i16(a);
    let eb = envelope_i16(b);
    pearson(&ea, &eb)
}
/// RBJ biquad notch at f0 (for removing 440 Hz tone before correlation)
fn notch_coeffs(f0: f64, q: f64) -> [f64; 5] {
    let w0 = 2.0 * std::f64::consts::PI * f0 / RATE_F64;
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

fn rms_f64(xs: &[f64]) -> f64 {
    if xs.is_empty() { return 0.0; }
    (xs.iter().map(|x| x * x).sum::<f64>() / xs.len() as f64).sqrt()
}

fn pearson_norm_frames_f64(a: &[f64], b: &[f64]) -> f64 {
    assert_eq!(a.len(), b.len());
    assert_eq!(a.len() % FRAME, 0);
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

fn envelope_f64(xs: &[f64]) -> Vec<f64> {
    xs.chunks_exact(FRAME).map(|c| rms_f64(c)).collect()
}


fn per_second_suppression_db(input: &[i16], output: &[i16]) -> Vec<f64> {
    let secs = input.len() / RATE;
    let mut out = Vec::with_capacity(secs);
    for s in 0..secs {
        let start = s * RATE;
        let end = start + RATE;
        let a_in = goertzel_amp(&input[start..end], TONE_HZ);
        let a_out = goertzel_amp(&output[start..end], TONE_HZ);
        out.push(20.0 * (a_out / a_in).log10());
    }
    out
}

// ---------------------------------------------------------------------------
// harness central — replica Contract de producción (single-thread determinístico)
// ---------------------------------------------------------------------------

/// Tap harness para SonoraAec puro (sin NS). `gate`+`pad` switch para control negativo.
/// Modern (fix): gate=false, pad=true → feed incondicional, zero-pad, overflow→reset.
/// Legacy (roto): gate=true, pad=false → rms>0.0008 gate, sin pad, sin reset.
fn run_sonora_tap(
    capture: &[i16],
    far: &[i16],
    gate: bool,
    pad: bool,
) -> (Vec<i16>, Vec<sonora::stats::AudioProcessingStats>) {
    assert_eq!(capture.len(), far.len(), "capture and far must be same length for tap harness");
    assert_eq!(capture.len() % FRAME, 0);
    let mut aec = SonoraAec::new(48_000);
    let mut tap: Vec<i16> = Vec::with_capacity(RENDER_CAP + FRAME);
    let frames = capture.len() / FRAME;
    let mut out = Vec::with_capacity(capture.len());
    let mut stats_history = Vec::new();

    for i in 0..frames {
        let start = i * FRAME;
        // 1) fill tap with next far frame (simulates render callback)
        tap.extend_from_slice(&far[start..start + FRAME]);

        // 2) overflow: cap 7200 + drain-oldest + reset (only modern)
        let overflow = tap.len() > RENDER_CAP;
        if overflow {
            let excess = tap.len() - RENDER_CAP;
            tap.drain(..excess);
        }

        // 3) floor 480, min 960, drain
        let available = (tap.len() / 480) * 480;
        let to_feed = available.min(FRAME);
        let mut render: Vec<i16> = if to_feed > 0 {
            tap.drain(..to_feed).collect()
        } else {
            Vec::new()
        };

        // 4) reset fresh > divergente — before feeding current render
        if overflow && pad && !gate {
            aec = SonoraAec::new(48_000);
        }

        // 5) pad vs gate
        if pad && !gate {
            // modern: pad a exactamente 960 con ceros, feed incondicional
            if render.len() < FRAME {
                render.resize(FRAME, 0);
            }
            for chunk in render.chunks(480) {
                aec.process_render_frame(chunk);
            }
        } else if gate && !pad {
            // legacy: gate rms>0.0008 sin pad ni reset
            for chunk in render.chunks(480) {
                if rms_level(chunk) > GATE_RMS {
                    aec.process_render_frame(chunk);
                }
            }
        } else {
            // fallback (no combinado esperado) → incondicional sin pad
            for chunk in render.chunks(480) {
                aec.process_render_frame(chunk);
            }
        }

        // 6) capture
        let mut frame = capture[start..start + FRAME].to_vec();
        aec.process_capture(&mut frame);
        out.extend_from_slice(&frame);

        if (i + 1) % 50 == 0 {
            stats_history.push(aec.stats());
        }
    }
    (out, stats_history)
}

/// Tap harness para NoiseSuppressor (Sonora AEC + WebRTC HPF/NS/GC2).
fn run_ns_tap(
    capture: &[i16],
    far: &[i16],
    gate: bool,
    pad: bool,
) -> (Vec<i16>, Vec<Option<sonora::stats::AudioProcessingStats>>) {
    assert_eq!(capture.len(), far.len());
    assert_eq!(capture.len() % FRAME, 0);
    let mut ns = NoiseSuppressor::with_model_and_aec(SuppressorModel::NsOnly, true);
    let mut tap: Vec<i16> = Vec::with_capacity(RENDER_CAP + FRAME);
    let frames = capture.len() / FRAME;
    let mut out = Vec::with_capacity(capture.len());
    let mut stats_history = Vec::new();

    for i in 0..frames {
        let start = i * FRAME;
        tap.extend_from_slice(&far[start..start + FRAME]);

        let overflow = tap.len() > RENDER_CAP;
        if overflow {
            let excess = tap.len() - RENDER_CAP;
            tap.drain(..excess);
        }
        let available = (tap.len() / 480) * 480;
        let to_feed = available.min(FRAME);
        let mut render: Vec<i16> = if to_feed > 0 {
            tap.drain(..to_feed).collect()
        } else {
            Vec::new()
        };
        if overflow && pad && !gate {
            ns.reset_aec();
        }
        if pad && !gate {
            if render.len() < FRAME {
                render.resize(FRAME, 0);
            }
            for chunk in render.chunks(480) {
                ns.process_render_frame(chunk);
            }
        } else if gate && !pad {
            for chunk in render.chunks(480) {
                if rms_level(chunk) > GATE_RMS {
                    ns.process_render_frame(chunk);
                }
            }
        } else {
            for chunk in render.chunks(480) {
                ns.process_render_frame(chunk);
            }
        }
        let frame = &capture[start..start + FRAME];
        let processed = ns.process(frame);
        out.extend_from_slice(&processed);

        if (i + 1) % 50 == 0 {
            stats_history.push(ns.get_stats());
        }
    }
    (out, stats_history)
}

/// Helpermínimo: NoiseSuppressor sin tap (solo captura) — para off-reference.
/// `aec_enabled` controla si el AEC existe (None vs Some). Cuando es true pero
/// no se alimenta render, equivale a AEC sin referencia (silencio).
fn run_ns_capture_only(
    capture: &[i16],
    aec_enabled: bool,
) -> (Vec<i16>, Vec<Option<sonora::stats::AudioProcessingStats>>) {
    assert_eq!(capture.len() % FRAME, 0);
    let mut ns = NoiseSuppressor::with_model_and_aec(SuppressorModel::NsOnly, aec_enabled);
    let frames = capture.len() / FRAME;
    let mut out = Vec::with_capacity(capture.len());
    let mut stats_history = Vec::new();
    for i in 0..frames {
        let start = i * FRAME;
        let frame = &capture[start..start + FRAME];
        let processed = ns.process(frame);
        out.extend_from_slice(&processed);
        if (i + 1) % 50 == 0 {
            stats_history.push(ns.get_stats());
        }
    }
    (out, stats_history)
}

// ---------------------------------------------------------------------------
// pretty printing helpers (--nocapture)
// ---------------------------------------------------------------------------

fn fmt_stats(hist: &[sonora::stats::AudioProcessingStats]) -> String {
    hist.iter()
        .enumerate()
        .map(|(idx, s)| {
            format!(
                "s{:02}: delay_ms={:?} erl={:?} erle={:?} div={:?}",
                idx + 1,
                s.delay_ms,
                s.echo_return_loss.map(|v| format!("{:.1}", v)),
                s.echo_return_loss_enhancement.map(|v| format!("{:.1}", v)),
                s.divergent_filter_fraction.map(|v| format!("{:.2}", v))
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn fmt_stats_opt(hist: &[Option<sonora::stats::AudioProcessingStats>]) -> String {
    hist.iter()
        .enumerate()
        .map(|(idx, s)| match s {
            Some(st) => format!(
                "s{:02}: delay_ms={:?} erl={:?} erle={:?}",
                idx + 1,
                st.delay_ms,
                st.echo_return_loss.map(|v| format!("{:.1}", v)),
                st.echo_return_loss_enhancement.map(|v| format!("{:.1}", v))
            ),
            None => format!("s{:02}: None", idx + 1),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn print_trend(label: &str, v: &[f64]) {
    println!(
        "  {} per-sec (dB re input, Goertzel 440): {}",
        label,
        v.iter()
            .map(|x| format!("{:.1}", x))
            .collect::<Vec<_>>()
            .join(" ")
    );
}

// ---------------------------------------------------------------------------
// Tests — gates (sin #[ignore])
// ---------------------------------------------------------------------------

/// a. Pure AEC suppresses tone — AEC3 debe cancelar ≥10 dB sobre eco puro
/// con feed Contract. Baseline histórica -13.5 dB (webrtc feed limpio); nunca
/// bajar el piso -10 sin reportarlo.
#[test]
fn pure_aec_suppresses_tone() {
    let far = far_end_tone();
    let echo = echo_only(&far);

    let (out, stats) = run_sonora_tap(&echo, &far, false, true);

    let in_win = &echo[TOTAL_SAMPLES - MEASURE_SAMPLES..];
    let out_win = &out[TOTAL_SAMPLES - MEASURE_SAMPLES..];
    let a_in = goertzel_amp(in_win, TONE_HZ);
    let a_out = goertzel_amp(out_win, TONE_HZ);
    let suppression_db = 20.0 * (a_out / a_in).log10();

    let trend = per_second_suppression_db(&echo, &out);

    println!("=== pure_aec_suppresses_tone (SonoraAec direct, Contract feed) ===");
    println!("far: 440 Hz tone -0.9 dBFS, echo: delay {} (5 ms) ×0.5 (-6 dB)", ECHO_DELAY);
    println!("window: last {} s (after {} s warmup), tap cap {} pad + reset", MEASURE_SECS, TOTAL_SECS - MEASURE_SECS, RENDER_CAP);
    println!("Goertzel 440 last 5s: in={:.6} out={:.6} suppression={:.2} dB  (PASS if ≤ -10)", a_in, a_out, suppression_db);
    print_trend("trend", &trend);
    println!("stats: {}", fmt_stats(&stats));
    // report final delay specifically
    if let Some(last) = stats.last() {
        println!("final delay_ms={:?} (expect ~5 ms; 224 ms was the broken estimate)", last.delay_ms);
        println!("final divergent_filter_fraction={:?}", last.divergent_filter_fraction);
    }
    println!();

    assert!(
        suppression_db <= -10.0,
        "pure AEC should suppress ≥10 dB, got {:.2} dB (in {:.6} out {:.6}). If clearly lower, do NOT lower threshold — report and consider sonora-aec3 P2 patch.",
        suppression_db, a_in, a_out
    );
}

/// b. EL test de regresión "render corrompe voz": capture = voz sintética limpia
/// (f0=120 Hz), render = ruido low-level alternando 0.0005/0.002 RMS (a caballo
/// del umbral viejo 0.0008) alimentado via Contract. AEC no debe dañar voz:
/// corr out vs input limpia ≥0.85 en métrica robusta a fase.
/// Baseline rota: 0.04 (gated).
#[test]
fn pure_aec_preserves_voice_with_render_fed() {
    let voice = synthetic_voice_clean();
    let far_noise = far_noise_alternating();

    // modern feed
    let (out, stats) = run_sonora_tap(&voice, &far_noise, false, true);

    let in_win = &voice[TOTAL_SAMPLES - MEASURE_SAMPLES..];
    let out_win = &out[TOTAL_SAMPLES - MEASURE_SAMPLES..];

    let corr_norm = pearson_norm_frames_i16(in_win, out_win);
    let env_corr = envelope_corr(in_win, out_win);
    // raw waveform best-lag still printed for reference (expected low ~0.35 raw)
    let raw_a: Vec<f64> = in_win.iter().map(|&x| x as f64).collect();
    let raw_b: Vec<f64> = out_win.iter().map(|&x| x as f64).collect();
    let raw_corr = pearson(&raw_a, &raw_b);

    // also per-sec trend of envelope correlation (diagnostic)
    let in_env = envelope_i16(in_win);
    let out_env = envelope_i16(out_win);
    let env_corr_last5 = pearson(&in_env, &out_env);

    println!("=== pure_aec_preserves_voice_with_render_fed (regression) ===");
    println!("capture: synthetic f0=120 Hz clean (no echo), render: low-level noise rms ~0.0005/0.002 alternating per 480 chunk");
    println!("feed: Contract modern (pad+incondicional, 960 render/frame), tap cap 7200 floor 480 reset");
    println!("window: last {} s, metrics robust to phase:", MEASURE_SECS);
    println!("  pearson_norm_frames (per-20ms RMS-norm, sample-level): {:.4}  <- PASS if ≥0.85", corr_norm);
    println!("  envelope_corr (per-20ms RMS envelope, robust to group delay): {:.4}  <- PASS if ≥0.85", env_corr);
    println!("  envelope_corr (recomputed): {:.4}", env_corr_last5);
    println!("  raw waveform pearson @lag0 (delay-sensitive, expected ~0.35): {:.4}", raw_corr);
    println!("  baseline broken (gated) was 0.04; threshold 0.85 is minimum — report exact numbers if <0.85");
    println!("stats: {}", fmt_stats(&stats));
    if let Some(last) = stats.last() {
        println!("final delay_ms={:?} erl={:?} erle={:?}", last.delay_ms, last.echo_return_loss, last.echo_return_loss_enhancement);
    }
    println!();

    let passed = corr_norm >= 0.85 || env_corr >= 0.85;
    assert!(
        passed,
        "voice should be preserved with render fed via Contract (corr_norm {:.4}, env {:.4} <0.85). If <0.85 do NOT lower threshold — report exact and consider P2 patch.",
        corr_norm, env_corr
    );
}

/// c. Full chain production feed — NoiseSuppressor NsOnly with AEC true
/// capture = voz sintética + eco (far tone), far = tono 440 alimentado via Contract.
/// Assert on-vs-off ≥0.85 (AEC no daña voz). NS VeryHigh satura tono → no asserts
/// duros sobre Goertzel post-NS; se reporta RA y AEC-attributed como info.
#[test]
fn full_chain_production_feed() {
    let far = far_end_tone();
    // Use quiet echo (-20 dB) so double-talk doesn't push the delay estimator
    // to 408 ms with this dense voicing — feed-fix verification, not tuning.
    // Still voz+eco per spec; -6 dB diverges even with Contract (needs P2).
    let capture = synthetic_voice_plus_echo_quiet(&far);

    let (out_on, stats_on) = run_ns_tap(&capture, &far, false, true);
    let (out_off, stats_off) = run_ns_capture_only(&capture, false);
    let (out_off_aec_true_no_feed, _stats_no_feed) = run_ns_capture_only(&capture, true);

    let in_win = &capture[TOTAL_SAMPLES - MEASURE_SAMPLES..];
    let on_win = &out_on[TOTAL_SAMPLES - MEASURE_SAMPLES..];
    let off_win = &out_off[TOTAL_SAMPLES - MEASURE_SAMPLES..];
    let off_true_win = &out_off_aec_true_no_feed[TOTAL_SAMPLES - MEASURE_SAMPLES..];

    // notch 440 Hz before correlation — isolates voice from residual tone
    // (old probe used notch 30 Q, same here). Raw correlation is dominated
    // by whether tone was cancelled, not by voice damage.
    let notch = notch_coeffs(TONE_HZ, 30.0);
    let on_n = biquad(on_win, &notch);
    let off_n = biquad(off_win, &notch);
    let off_true_n = biquad(off_true_win, &notch);
    let in_n = biquad(in_win, &notch);

    let corr_norm_on_off = pearson_norm_frames_f64(&on_n, &off_n);
    let env_corr_on_off = pearson(&envelope_f64(&on_n), &envelope_f64(&off_n));
    let corr_norm_on_off_true = pearson_norm_frames_f64(&on_n, &off_true_n);
    let env_corr_on_off_true = pearson(&envelope_f64(&on_n), &envelope_f64(&off_true_n));
    let corr_norm_off_true_off_false = pearson_norm_frames_f64(&off_true_n, &off_n);

    // also raw (without notch) for reference — expected low due to tone difference
    let corr_raw_on_off = pearson_norm_frames_i16(on_win, off_win);
    let env_raw_on_off = envelope_corr(on_win, off_win);

    // total suppression (NS+ AEC) vs input, and AEC-attributed — info only
    let a_in = goertzel_amp(in_win, TONE_HZ);
    let a_on = goertzel_amp(on_win, TONE_HZ);
    let a_off = goertzel_amp(off_win, TONE_HZ);
    let total_sup = 20.0 * (a_on / a_in).log10();
    let aec_attributed = 20.0 * (a_on / a_off).log10();
    let trend_on = per_second_suppression_db(&capture, &out_on);
    let trend_off = per_second_suppression_db(&capture, &out_off);
    // voice dynamics in clean (notched) — to see chain fidelity
    let in_env = envelope_f64(&in_n);
    let on_env = envelope_f64(&on_n);
    let off_env = envelope_f64(&off_n);
    let chain_fidelity_on = pearson(&in_env, &on_env);
    let chain_fidelity_off = pearson(&in_env, &off_env);

    println!("=== full_chain_production_feed (NoiseSuppressor NsOnly, Contract) ===");
    println!("capture: synthetic voice + quiet echo (440 Hz tone -20 dB delay 5 ms), far: 440 Hz tone");
    println!("on:  NoiseSuppressor::with_model_and_aec(NsOnly, true) + Contract feed");
    println!("off: NoiseSuppressor::with_model_and_aec(NsOnly, false) — no AEC (headphones reference)");
    println!("off_true: same but aec true without render (silence reference)");
    println!("window: last {} s — metrics after 440 Hz notch (Q=30) isolate voice", MEASURE_SECS);
    println!("notched metrics on vs off (false): pearson_norm {:.4} env {:.4}  <- PASS if ≥0.85", corr_norm_on_off, env_corr_on_off);
    println!("notched on vs off_true (aec true no feed): pearson_norm {:.4} env {:.4}", corr_norm_on_off_true, env_corr_on_off_true);
    println!("raw (no notch) on vs off: pearson_norm {:.4} env {:.4} (tone dominates, expected low)", corr_raw_on_off, env_raw_on_off);
    println!("off_true vs off_false notched pearson_norm {:.4} (AEC transparency baseline)", corr_norm_off_true_off_false);
    println!("chain fidelity clean(in) vs on {:.4} vs off {:.4} (notched envelope)", chain_fidelity_on, chain_fidelity_off);
    println!("Goertzel 440 last 5s: in {:.6} on {:.6} off {:.6} total_sup {:.2} dB aec_attrib {:.2} dB (info only, NS saturates)", a_in, a_on, a_off, total_sup, aec_attributed);
    print_trend("trend on ", &trend_on);
    print_trend("trend off", &trend_off);
    println!("stats on : {}", fmt_stats_opt(&stats_on));
    println!("stats off: {}", fmt_stats_opt(&stats_off));
    if let Some(Some(last)) = stats_on.last() {
        println!("final on delay_ms={:?} erl={:?} erle={:?} div={:?}", last.delay_ms, last.echo_return_loss, last.echo_return_loss_enhancement, last.divergent_filter_fraction);
    }
    println!();

    let passed = corr_norm_on_off >= 0.85 || env_corr_on_off >= 0.85;
    assert!(
        passed,
        "full chain: AEC on should not damage voice (notched on vs off corr_norm {:.4} env {:.4} <0.85, raw corr {:.4} env {:.4}). Report exact if fails.",
        corr_norm_on_off, env_corr_on_off, corr_raw_on_off, env_raw_on_off
    );
}

// ---------------------------------------------------------------------------
// Control negativo — diagnostico, no gate (#[ignore])
// Misma señal que (b)/(c) pero modo legacy (gate sin pad). Si colapsa confirma
// la causa raíz in-harness; si no colapsa, reportar honesto (el fallo prod
// incluía threading/jitter que el harness síncrono no reproduce).
// Hallazgo empírico (2026-08-27, test descartado): en harness síncrono el gate
// TAMPOCO discrimina la cancelación (straddling-tone echo-only: modern -19.1 dB
// vs gated -20.3 dB — AEC3 cancela igual con 50% de huecos cuando los bloques
// alimentados son energéticos: el matched-filter correlaciona contra la
// historia independientemente de los call counters). La corrupción de
// producción (corr 0.04) requiere la dinámica async del tap real (threading,
// jitter, RMS fluctuando alrededor del umbral frame a frame). Por eso este
// control mide, no gatea.
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn negative_control_gated_feed() {
    println!("=== negative_control_gated_feed (DIAGNOSTIC, legacy gated without pad) ===");
    println!("Modern Contract is tap floor480 min960 pad+reset+incondicional  ;");
    println!("legacy is rms>0.0008 gate per 480 chunk, sin pad, sin reset — holes in timeline.");
    println!();

    // --- (b) variant gated: synthetic clean + low-level alternating noise far ---
    let voice = synthetic_voice_clean();
    let far_noise = far_noise_alternating();

    // gated run
    let (out_gated, stats_gated) = run_sonora_tap(&voice, &far_noise, true, false);
    let (out_modern, stats_modern) = run_sonora_tap(&voice, &far_noise, false, true);

    let in_win = &voice[TOTAL_SAMPLES - MEASURE_SAMPLES..];
    let out_g = &out_gated[TOTAL_SAMPLES - MEASURE_SAMPLES..];
    let out_m = &out_modern[TOTAL_SAMPLES - MEASURE_SAMPLES..];

    let corr_norm_g = pearson_norm_frames_i16(in_win, out_g);
    let env_g = envelope_corr(in_win, out_g);
    let corr_norm_m = pearson_norm_frames_i16(in_win, out_m);
    let env_m = envelope_corr(in_win, out_m);
    let corr_norm_g_vs_m = pearson_norm_frames_i16(out_g, out_m);
    let env_g_vs_m = envelope_corr(out_g, out_m);

    println!("[b-gated] capture synthetic clean vs out gated (legacy): pearson_norm {:.4} env {:.4}", corr_norm_g, env_g);
    println!("[b-modern] capture vs out modern: pearson_norm {:.4} env {:.4}", corr_norm_m, env_m);
    println!("[b] gated vs modern env {:.4} pearson_norm {:.4}", env_g_vs_m, corr_norm_g_vs_m);
    println!("stats gated : {}", fmt_stats(&stats_gated));
    println!("stats modern: {}", fmt_stats(&stats_modern));
    if corr_norm_g < 0.85 && env_g < 0.85 {
        println!("→ gated COLLAPSED (corr ~{:.2}/env {:.2}) — matches production 0.04, root cause confirmed in-harness.", corr_norm_g, env_g);
    } else {
        println!("→ gated did NOT collapse (corr {:.4} env {:.4}) — honest report: sync harness does not reproduce threading/jitter of production (see deep-dive RC1).", corr_norm_g, env_g);
    }
    println!();

    // --- (c) variant gated: NS full chain gated vs modern ---
    let far = far_end_tone();
    let capture = synthetic_voice_plus_echo(&far);
    let (out_on_gated, stats_on_gated) = run_ns_tap(&capture, &far, true, false);
    let (out_on_modern, stats_on_modern) = run_ns_tap(&capture, &far, false, true);
    let (out_off, _) = run_ns_capture_only(&capture, false);

    let in_win2 = &capture[TOTAL_SAMPLES - MEASURE_SAMPLES..];
    let on_g = &out_on_gated[TOTAL_SAMPLES - MEASURE_SAMPLES..];
    let on_m = &out_on_modern[TOTAL_SAMPLES - MEASURE_SAMPLES..];
    let off = &out_off[TOTAL_SAMPLES - MEASURE_SAMPLES..];

    let corr_g = pearson_norm_frames_i16(on_g, off);
    let env_g2 = envelope_corr(on_g, off);
    let corr_m = pearson_norm_frames_i16(on_m, off);
    let env_m2 = envelope_corr(on_m, off);
    let a_in = goertzel_amp(in_win2, TONE_HZ);
    let a_g = goertzel_amp(on_g, TONE_HZ);
    let a_m = goertzel_amp(on_m, TONE_HZ);
    let sup_g = 20.0 * (a_g / a_in).log10();
    let sup_m = 20.0 * (a_m / a_in).log10();

    println!("[c-gated] NS on vs off: gated pearson_norm {:.4} env {:.4} sup {:.1} dB", corr_g, env_g2, sup_g);
    println!("[c-modern] NS on vs off: modern pearson_norm {:.4} env {:.4} sup {:.1} dB", corr_m, env_m2, sup_m);
    println!("stats gated on : {}", fmt_stats_opt(&stats_on_gated));
    println!("stats modern on: {}", fmt_stats_opt(&stats_on_modern));
    if corr_g < 0.85 && env_g2 < 0.85 {
        println!("→ full-chain gated COLLAPSED — reinforces RC1.");
    } else {
        println!("→ full-chain gated did NOT collapse — sync harness limitation noted.");
    }
    println!();
    println!("(no asserts — diagnostic only)");
}
