//! GTCRN probe — measures the REAL send-path DSP chain (NoiseSuppressor:
//! WebRTC AEC3 + NS, then RNNoise, then GTCRN, then the VAD leveler) on a
//! deterministic noisy-speech signal, and prints the noise reduction (dB) and
//! speech preservation (dB) it actually achieves. Run with
//! `cargo test -p lumen-voice --test gtcrn_probe -- --nocapture` (it also runs
//! in CI via the `voice-probe` job) so the numbers are visible.
//!
//! This is a headless, deterministic probe: it synthesizes the input (no real
//! mic), so it can run anywhere and gives repeatable numbers. It answers "is
//! the denoiser actually suppressing noise on this build/host, and is it
//! eating the speech?"

use lumen_voice::audio::NoiseSuppressor;

const RATE: u32 = 48_000;
/// 20 ms capture frame (the app's real frame size).
const FRAME: usize = (RATE as usize * 20) / 1000; // 960
/// Total probe duration.
const DUR_S: f64 = 3.0;
const N_FRAMES: usize = (DUR_S * RATE as f64) as usize / FRAME;

/// Deterministic LCG noise (fixed seed) — reproducible across runs/hosts.
struct Lcg(u64);
impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        // Map to [-1, 1).
        (self.0 >> 33) as f32 / (1u32 << 31) as f32
    }
}

/// Speech-like tone stack (a few "formants") with a syllable envelope.
fn speech_sample(t: f64) -> f32 {
    // 0.5 s syllable period: burst then pause (like talking).
    let env = (std::f64::consts::PI * 2.0 * t / 0.5).sin().max(0.0).powi(2);
    let formants = 0.55 * (std::f64::consts::TAU * 180.0 * t).sin()
        + 0.30 * (std::f64::consts::TAU * 540.0 * t).sin()
        + 0.15 * (std::f64::consts::TAU * 2160.0 * t).sin();
    (formants * env) as f32
}

fn rms(v: &[f32]) -> f64 {
    let sum: f64 = v.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    (sum / v.len() as f64).sqrt()
}

fn to_i16(v: &[f32]) -> Vec<i16> {
    v.iter()
        .map(|s| (s * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
        .collect()
}

fn db_ratio(a: f64, b: f64) -> f64 {
    if b <= 0.0 || a <= 0.0 {
        0.0
    } else {
        20.0 * (a / b).log10()
    }
}

#[test]
fn gtcrn_probe() {
    // --- Build the deterministic signal -------------------------------------------------
    // Speech at a fixed peak; noise scaled to hit a target input SNR.
    let mut speech = vec![0f32; N_FRAMES * FRAME];
    for (i, s) in speech.iter_mut().enumerate() {
        *s = speech_sample(i as f64 / RATE as f64);
    }
    let speech_rms = rms(&speech);
    let target_snr_db = 10.0; // 10 dB SNR input — noisy but audible.
    let noise_target = speech_rms / 10f64.powf(target_snr_db / 20.0);

    let mut lcg = Lcg(0x5EED_CAFE);
    let mut noise = vec![0f32; N_FRAMES * FRAME];
    for s in noise.iter_mut() {
        *s = lcg.next_f32() * noise_target as f32;
    }

    // Per-frame envelope peak, to classify speech vs silence frames.
    let mut is_speech = vec![false; N_FRAMES];
    for f in 0..N_FRAMES {
        let t0 = f as f64 * FRAME as f64 / RATE as f64;
        let env = (std::f64::consts::PI * 2.0 * t0 / 0.5).sin().max(0.0).powi(2);
        is_speech[f] = env > 0.05;
    }

    let mix: Vec<i16> = {
        let m: Vec<f32> = speech.iter().zip(&noise).map(|(s, n)| s + n).collect();
        to_i16(&m)
    };

    // --- Run through the REAL app chain -------------------------------------------------
    let mut noisy_ns = NoiseSuppressor::new();
    let mut clean_ns = NoiseSuppressor::new();
    let gtcrn_active = noisy_ns.gtcrn_active();

    let mut out_noisy = Vec::with_capacity(mix.len());
    for (f, chunk) in mix.chunks_exact(FRAME).enumerate() {
        let _ = f;
        out_noisy.extend(noisy_ns.process(chunk));
    }
    let out_clean: Vec<i16> = clean_ns
        .process(&to_i16(&speech))
        .into_iter()
        .collect();

    // --- Measure ------------------------------------------------------------------------
    // Noise reduction: input vs output RMS over the SILENCE frames (denoiser
    // should crush the floor there).
    let mut in_noise: Vec<f32> = Vec::new();
    let mut out_noise: Vec<f32> = Vec::new();
    for (f, chunk) in mix.chunks_exact(FRAME).enumerate() {
        if !is_speech[f] {
            for (i, s) in chunk.iter().enumerate() {
                in_noise.push(*s as f32 / 32768.0);
                out_noise.push(out_noisy[f * FRAME + i] as f32 / 32768.0);
            }
        }
    }
    let in_noise_rms = rms(&in_noise);
    let out_noise_rms = rms(&out_noise);
    let noise_reduction_db = db_ratio(in_noise_rms, out_noise_rms);

    // Speech preservation: clean-path output vs input over the SPEECH frames.
    let mut in_speech: Vec<f32> = Vec::new();
    let mut out_speech: Vec<f32> = Vec::new();
    for (f, chunk) in speech.chunks_exact(FRAME).enumerate() {
        if is_speech[f] {
            for (i, s) in chunk.iter().enumerate() {
                in_speech.push(*s);
                out_speech.push(out_clean[f * FRAME + i] as f32 / 32768.0);
            }
        }
    }
    let speech_in_rms = rms(&in_speech);
    let speech_out_rms = rms(&out_speech);
    let speech_preservation_db = db_ratio(speech_out_rms, speech_in_rms);

    // --- Report -------------------------------------------------------------------------
    println!();
    println!("=== GTCRN PROBE (real NoiseSuppressor chain) ===");
    println!("GTCRN (sherpa-onnx) active : {}", gtcrn_active);
    println!("input SNR (synthetic)      : {target_snr_db:.1} dB");
    println!("input noise floor          : {:.1} dBFS", db_ratio(in_noise_rms, 1.0));
    println!("output noise floor         : {:.1} dBFS", db_ratio(out_noise_rms, 1.0));
    println!("NOISE REDUCTION            : {noise_reduction_db:.1} dB  (want >= 20 for GTCRN, ~9 for RNNoise-only)");
    println!("SPEECH PRESERVATION        : {speech_preservation_db:+.1} dB  (want >= -3)");
    println!("=== end probe ===");
    println!();

    // Assertions (loose ceilings so the probe fails loudly if the chain breaks,
    // but doesn't flake on CI).
    assert!(
        noise_reduction_db > 6.0,
        "denoiser not suppressing noise: {noise_reduction_db:.1} dB"
    );
    assert!(
        speech_preservation_db > -8.0,
        "denoiser destroying speech: {speech_preservation_db:.1} dB"
    );
    if gtcrn_active {
        // GTCRN claims 32-38 dB; demand a meaningful step over the RNNoise
        // fallback (~9 dB from WebRTC NS + RNNoise). Loose to avoid flake.
        assert!(
            noise_reduction_db > 15.0,
            "GTCRN active but only {noise_reduction_db:.1} dB reduction"
        );
    }
}
