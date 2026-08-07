//! GTCRN probe — measures the REAL send-path DSP chain (NoiseSuppressor:
//! WebRTC AEC3 + NS, then RNNoise, then GTCRN, then the VAD leveler) on real
//! speech + deterministic noise, and prints what it actually achieves.
//! Run with `cargo test -p lumen-voice --test gtcrn_probe -- --nocapture`
//! (also runs in CI via the `voice-probe` job) so the numbers are visible.
//!
//! All three metrics are LATENCY-AGNOSTIC (global RMS over a whole stream),
//! because GTCRN + resampling delay the output by ~80 ms — any frame-aligned
//! measurement reads the delayed tail and misreports speech as silence.
//!
//!   - NOISE REDUCTION: pure noise through the chain, input vs output RMS (dB).
//!   - SPEECH PRESERVATION: real speech (committed wav) through the chain,
//!     output vs input RMS (dB; want >= -3).
//!   - REALISTIC OUTPUT SNR: the level of preserved speech vs the residual
//!     noise the chain leaves, both measured globally (dB).
//!
//! GTCRN is a neural net trained on natural speech, so the probe uses a real
//! speech fixture (testdata/speech.wav, espeak-synthesized with inter-word
//! pauses), not synthetic tones — those read as "noise" to the model.

use lumen_voice::audio::NoiseSuppressor;

const RATE: u32 = 48_000;
/// 20 ms capture frame (the app's real frame size).
const FRAME: usize = (RATE as usize * 20) / 1000; // 960

/// Deterministic LCG noise (fixed seed) — reproducible across runs/hosts.
struct Lcg(u64);
impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as f32 / (1u32 << 31) as f32
    }
}

fn rms_i16(v: &[i16]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let sum: f64 = v.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    (sum / v.len() as f64).sqrt()
}

fn db_ratio(a: f64, b: f64) -> f64 {
    if b <= 1e-9 || a <= 1e-9 {
        0.0
    } else {
        20.0 * (a / b).log10()
    }
}

/// Parse a 16-bit mono PCM WAV (embedded fixture) into f32 samples in [-1, 1).
fn load_wav() -> Vec<f32> {
    let wav: &[u8] = include_bytes!("../testdata/speech.wav");
    assert!(wav[0..4] == *b"RIFF" && wav[8..12] == *b"WAVE", "not a wav");
    let mut off = 12;
    let mut pcm = Vec::new();
    while off + 8 <= wav.len() {
        let id = &wav[off..off + 4];
        let size = u32::from_le_bytes(wav[off + 4..off + 8].try_into().unwrap()) as usize;
        let body = off + 8;
        if id == b"fmt " {
            let rate = u32::from_le_bytes(wav[body + 4..body + 8].try_into().unwrap());
            let channels = u16::from_le_bytes(wav[body + 2..body + 4].try_into().unwrap());
            let bits = u16::from_le_bytes(wav[body + 14..body + 16].try_into().unwrap());
            assert_eq!(rate, RATE, "fixture must be 48 kHz");
            assert_eq!(channels, 1, "fixture must be mono");
            assert_eq!(bits, 16, "fixture must be 16-bit");
        } else if id == b"data" {
            let n = size / 2;
            for i in 0..n {
                let s = i16::from_le_bytes(wav[body + i * 2..body + i * 2 + 2].try_into().unwrap());
                pcm.push(s as f32 / 32768.0);
            }
        }
        off = body + size + (size & 1); // chunks are word-aligned
    }
    assert!(!pcm.is_empty(), "no data chunk");
    // Trim to a whole number of 20 ms frames.
    pcm.truncate((pcm.len() / FRAME) * FRAME);
    pcm
}

fn to_i16(v: &[f32]) -> Vec<i16> {
    v.iter()
        .map(|s| (s * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
        .collect()
}

#[test]
fn gtcrn_probe() {
    let mut speech = load_wav();
    // Normalize to a realistic peak (~ -12 dBFS), which the chain handles
    // cleanly; keep it deterministic.
    let peak = speech.iter().fold(0.0f32, |m, s| m.max(s.abs())).max(1e-6);
    for s in speech.iter_mut() {
        *s *= 0.25 / peak;
    }
    let n = speech.len();
    let speech_rms = rms_i16(&to_i16(&speech));

    // --- Deterministic noise at a target input SNR ---------------------------
    let target_snr_db = 10.0;
    let noise_rms = speech_rms / 10f64.powf(target_snr_db / 20.0);
    let mut lcg = Lcg(0x5EED_CAFE);
    let noise: Vec<i16> = (0..n).map(|_| (lcg.next_f32() * noise_rms as f32) as i16).collect();

    let speech_i16 = to_i16(&speech);
    let mix_i16: Vec<i16> = speech_i16.iter().zip(&noise).map(|(s, n)| *s + *n).collect();

    // --- Run through the REAL app chain (separate instances per path) --------
    let mut ns_clean = NoiseSuppressor::new();
    let mut ns_noise = NoiseSuppressor::new();
    let gtcrn_active = ns_clean.gtcrn_active();

    let out_clean: Vec<i16> = speech_i16.chunks_exact(FRAME).flat_map(|f| ns_clean.process(f)).collect();
    let out_noise: Vec<i16> = noise.chunks_exact(FRAME).flat_map(|f| ns_noise.process(f)).collect();

    // --- Metric 1: NOISE REDUCTION (pure noise, global RMS) ------------------
    let noise_reduction_db = db_ratio(rms_i16(&noise), rms_i16(&out_noise));

    // --- Metric 2: SPEECH PRESERVATION (clean path, global RMS) --------------
    let speech_preservation_db = db_ratio(rms_i16(&out_clean), rms_i16(&speech_i16));

    // --- Metric 3: REALISTIC OUTPUT SNR (preserved speech vs residual noise) --
    let speech_out_rms = rms_i16(&out_clean);
    let noise_out_rms = rms_i16(&out_noise);
    let realistic_output_snr_db = db_ratio(speech_out_rms, noise_out_rms);

    // --- Report ----------------------------------------------------------------
    println!();
    println!("=== GTCRN PROBE (real NoiseSuppressor chain, real speech) ===");
    println!("GTCRN (sherpa-onnx) active : {}", gtcrn_active);
    println!("input SNR (synthetic mix)  : {target_snr_db:.1} dB");
    println!("NOISE REDUCTION            : {noise_reduction_db:.1} dB  (want >= 20 for GTCRN)");
    println!("SPEECH PRESERVATION        : {speech_preservation_db:+.1} dB  (want >= -3)");
    println!("REALISTIC OUTPUT SNR       : {realistic_output_snr_db:.1} dB  (was {target_snr_db:.1} dB in)");
    println!("=== end probe ===");
    println!();

    // Fail loudly if the chain is broken, but keep ceilings loose for CI.
    assert!(noise_reduction_db > 6.0, "denoiser not suppressing noise: {noise_reduction_db:.1} dB");
    assert!(speech_preservation_db > -8.0, "denoiser destroying speech: {speech_preservation_db:.1} dB");
    if gtcrn_active {
        assert!(noise_reduction_db > 15.0, "GTCRN active but only {noise_reduction_db:.1} dB");
        assert!(speech_preservation_db > -4.0, "GTCRN eating real speech: {speech_preservation_db:.1} dB");
        assert!(realistic_output_snr_db > target_snr_db + 8.0,
            "GTCRN should improve output SNR by >= 8 dB; got {realistic_output_snr_db:.1} vs {target_snr_db:.1} in");
    }
}
