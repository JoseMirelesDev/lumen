//! fe-ab probe: run FastEnhancer-Small / FastEnhancer-Base (48 kHz int8,
//! hop 512) through the SAME send chain as the app — WebRTC APM (HPF + NS
//! VeryHigh + GC2, AEC off) + the FE denoiser + peak limiter — on the hard
//! 36 s input, measure RTF per variant and write WAVs for A/B listening.
//!
//! The APM config below is a literal copy of
//! lumen-voice src/audio.rs `apm_config_with_aec(false)` (private there);
//! limit_peaks/rms_level are copies too. The FE wrapper accumulates the
//! 960-sample app frames into the runtime's 512-sample stream frames
//! (hop 512 -> 10.67 ms) with a residual buffer.
//!
//! Run (from the repo root, after `cargo build --release`):
//!   CARGO_TARGET_DIR=target cargo run --release -p fe-ab --manifest-path crates/fe-ab/Cargo.toml -- --variant both
use std::time::{Duration, Instant};

use webrtc_audio_processing::config::{
    AdaptiveDigital, Config, EchoCanceller, FixedDigital, GainController, GainController2,
    HighPassFilter, NoiseSuppression, NoiseSuppressionLevel,
};
use webrtc_audio_processing::Processor;

const RATE: u32 = 48_000;
const FE_FRAME: usize = 512; // hop 512 @ 48 kHz = 10.67 ms

// ---------------------------------------------------------------------------
// FFI to the two symbol-renamed runtime builds (fe_s_* / fe_b_*).
// ---------------------------------------------------------------------------
#[link(name = "fe_s")]
extern "C" {
    fn fe_s_init(weights: *const std::os::raw::c_void, size: std::os::raw::c_int) -> std::os::raw::c_int;
    fn fe_s_run(input: *const f32, output: *mut f32);
    fn fe_s_free();
    fn fe_s_fe_profile_dump(num_frames: std::os::raw::c_int);
}
#[link(name = "fe_b")]
extern "C" {
    fn fe_b_init(weights: *const std::os::raw::c_void, size: std::os::raw::c_int) -> std::os::raw::c_int;
    fn fe_b_run(input: *const f32, output: *mut f32);
    fn fe_b_free();
    fn fe_b_fe_profile_dump(num_frames: std::os::raw::c_int);
}
#[link(name = "fe_m")]
extern "C" {
    fn fe_m_init(weights: *const std::os::raw::c_void, size: std::os::raw::c_int) -> std::os::raw::c_int;
    fn fe_m_run(input: *const f32, output: *mut f32);
    fn fe_m_free();
    fn fe_m_fe_profile_dump(num_frames: std::os::raw::c_int);
}

const WEIGHTS_S: &[u8] = include_bytes!("../weights/fe_s.q8");
const WEIGHTS_B: &[u8] = include_bytes!("../weights/fe_b.q8");
const WEIGHTS_M: &[u8] = include_bytes!("../weights/fe_m.q8");

// ---------------------------------------------------------------------------
// FE wrapper: 960-sample app frames -> per-model stream frames
// (512 for Small/Base = 10.67 ms; 320 for Medium = 6.67 ms).
// ---------------------------------------------------------------------------
struct FeEngine {
    run: unsafe extern "C" fn(*const f32, *mut f32),
    free: unsafe extern "C" fn(),
    buf: Vec<f32>,
    start: usize,
    frame: usize,
    fe_time: Duration,
    fe_calls: u64,
}

impl FeEngine {
    fn new(
        init: unsafe extern "C" fn(*const std::os::raw::c_void, std::os::raw::c_int) -> std::os::raw::c_int,
        run: unsafe extern "C" fn(*const f32, *mut f32),
        free: unsafe extern "C" fn(),
        weights: &'static [u8],
        frame: usize,
    ) -> Option<Self> {
        let ok = unsafe { init(weights.as_ptr() as *const std::os::raw::c_void, weights.len() as std::os::raw::c_int) };
        if ok != 0 {
            return None;
        }
        Some(Self::from_init(run, free, frame))
    }

    /// Wrap an already-initialized engine (init handled by the caller).
    fn from_init(
        run: unsafe extern "C" fn(*const f32, *mut f32),
        free: unsafe extern "C" fn(),
        frame: usize,
    ) -> Self {
        Self { run, free, buf: Vec::new(), start: 0, frame, fe_time: Duration::ZERO, fe_calls: 0 }
    }

    /// Feed one 20 ms frame; returns the denoised samples emitted so far
    /// (`frame` per runtime call; the residual tail is dropped at the end).
    fn process(&mut self, frame_in: &[i16]) -> Vec<i16> {
        for &s in frame_in {
            self.buf.push(s as f32 / 32768.0);
        }
        let f = self.frame;
        let mut out = Vec::new();
        let mut chunk = vec![0f32; f];
        let mut denoised = vec![0f32; f];
        while self.buf.len() - self.start >= f {
            chunk.copy_from_slice(&self.buf[self.start..self.start + f]);
            let t = Instant::now();
            unsafe { (self.run)(chunk.as_ptr(), denoised.as_mut_ptr()) };
            self.fe_time += t.elapsed();
            self.fe_calls += 1;
            self.start += f;
            for &v in denoised.iter() {
                out.push((v * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
            }
        }
        if self.start > 1 << 20 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        out
    }
}

impl Drop for FeEngine {
    fn drop(&mut self) {
        unsafe { (self.free)() }
    }
}

// ---------------------------------------------------------------------------
// Chain helpers (copies of lumen-voice audio.rs — private there).
// ---------------------------------------------------------------------------
fn rms_level(pcm: &[i16]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let mut sum = 0f64;
    for &s in pcm {
        let d = s as f64 / 32768.0;
        sum += d * d;
    }
    (sum / pcm.len() as f64).sqrt() as f32
}

fn limit_peaks(samples: &mut [i16], ceiling_db: f32) {
    let ceiling = 10f32.powf(-ceiling_db / 20.0) * 32767.0;
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as f32;
    if peak > ceiling {
        let gain = ceiling / peak;
        for s in samples.iter_mut() {
            *s = ((*s as f32) * gain).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
    }
}

fn apm_config() -> Config {
    Config {
        echo_canceller: None, // aec_enabled=false (user's settings)
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

// ---------------------------------------------------------------------------
// WAV I/O (same minimal 44-byte header as the lumen-voice probes).
// ---------------------------------------------------------------------------
fn load_wav(path: &str) -> Vec<i16> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let mut b = Vec::new();
    f.read_to_end(&mut b).unwrap();
    b[44..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

fn write_wav(path: &str, pcm: &[i16]) {
    let n = pcm.len() as u32;
    let mut buf = Vec::with_capacity(44 + pcm.len() * 2);
    buf.extend(b"RIFF");
    buf.extend((36 + n * 2).to_le_bytes());
    buf.extend(b"WAVEfmt ");
    buf.extend(16u32.to_le_bytes());
    buf.extend(1u16.to_le_bytes());
    buf.extend(1u16.to_le_bytes());
    buf.extend(RATE.to_le_bytes());
    buf.extend((RATE * 2).to_le_bytes());
    buf.extend(2u16.to_le_bytes());
    buf.extend(16u16.to_le_bytes());
    buf.extend(b"data");
    buf.extend((n * 2).to_le_bytes());
    for s in pcm {
        buf.extend(s.to_le_bytes());
    }
    std::fs::write(path, &buf).unwrap_or_else(|e| panic!("write {path}: {e}"));
}

// ---------------------------------------------------------------------------
// Probe body.
// ---------------------------------------------------------------------------
fn run_variant(label: &str, mut fe: Option<&mut FeEngine>, input: &[i16], out_path: &str) {
    let audio_secs = input.len() as f64 / RATE as f64;
    let mut proc = Processor::new(RATE).expect("APM init");
    proc.set_config(apm_config());

    let mut out_all: Vec<i16> = Vec::with_capacity(input.len());
    let mut apm_time = Duration::ZERO;
    let t0 = Instant::now();

    for frame in input.chunks_exact(960) {
        // 1. WebRTC APM (HPF + NS VeryHigh + GC2), 480-sample blocks.
        let mut apm_out = vec![0i16; 960];
        let mut buf = [0f32; 480];
        let ta = Instant::now();
        for (ic, oc) in frame.chunks_exact(480).zip(apm_out.chunks_exact_mut(480)) {
            for (i, s) in ic.iter().enumerate() {
                buf[i] = *s as f32 / 32768.0;
            }
            if proc.process_capture_frame([&mut buf]).is_ok() {
                for (i, v) in buf.iter().enumerate() {
                    oc[i] = (v * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                }
            } else {
                oc.copy_from_slice(ic);
            }
        }
        apm_time += ta.elapsed();
        // 2. FE denoiser (timed inside the engine); None = NS-only chain.
        let mut denoised = if let Some(f) = fe.as_deref_mut() {
            f.process(&apm_out)
        } else {
            apm_out
        };
        // 3. Peak limiter (the production safety net).
        limit_peaks(&mut denoised, 1.0);
        out_all.extend(denoised);
    }
    let wall = t0.elapsed();

    write_wav(out_path, &out_all);

    // Metrics (same as the lumen-voice probes): floor p10 / voice p90 of
    // 100 ms RMS windows, pumping p90 of per-frame deltas.
    let rms = |x: &[i16]| -> f32 {
        (x.iter().map(|&s| (s as i64) * (s as i64)).sum::<i64>() as f64 / x.len() as f64).sqrt() as f32
    };
    let win = 4800usize;
    let mut owins: Vec<f32> = out_all.chunks(win).map(rms).collect();
    owins.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let floor = owins[owins.len() / 10];
    let voice = owins[(owins.len() as f32 * 0.9) as usize];
    let frames: Vec<f32> = out_all.chunks(960).map(rms).collect();
    let mut deltas: Vec<f32> = frames
        .windows(2)
        .filter(|w| w[0] > 0.02 && w[1] > 0.02)
        .map(|w| (w[1] - w[0]).abs() / w[0].max(1e-6))
        .collect();
    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pump = deltas[(deltas.len() as f32 * 0.9) as usize];

    let rtf_apm = apm_time.as_secs_f64() / audio_secs;
    let (rtf_fe, frame_ms, frame_nom_ms, fe_calls) = match &fe {
        Some(f) => (
            f.fe_time.as_secs_f64() / audio_secs,
            f.fe_time.as_secs_f64() / f.fe_calls.max(1) as f64 * 1000.0,
            f.frame as f64 / RATE as f64 * 1000.0,
            f.fe_calls,
        ),
        None => (0.0, 0.0, 0.0, 0),
    };
    let rtf_chain = rtf_apm + rtf_fe;
    let rtf_wall = wall.as_secs_f64() / audio_secs;

    println!("=== {label} ===");
    if fe.is_some() {
        println!("  fe frames: {} ({:.1} s audio in, {:.3} s out)",
            fe_calls, audio_secs, out_all.len() as f64 / RATE as f64);
        println!("  fe_run p50: {:.3} ms / {:.2} ms frame  ->  RTF(fe) {:.3}",
            frame_ms, frame_nom_ms, rtf_fe);
    } else {
        println!("  sin FE ({:.1} s audio in, {:.3} s out)", audio_secs, out_all.len() as f64 / RATE as f64);
    }
    println!("  RTF(apm) {:.4} | RTF(chain) {:.4} | wall {:.4}",
        rtf_apm, rtf_chain, rtf_wall);
    println!("  floor {:.1} dBFS | voice p90 {:.1} dBFS | pumping {:.3}",
        20.0 * (floor / 32768.0).log10(), 20.0 * (voice / 32768.0).log10(), pump);
    println!("  wrote {out_path}");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let variant = args.iter().find_map(|a| a.strip_prefix("--variant=").map(String::from))
        .unwrap_or_else(|| "both".to_string());
    let input = args.iter().find_map(|a| a.strip_prefix("--input=").map(String::from))
        .unwrap_or_else(|| "samples/harness_fe_hard36_input.wav".to_string());
    let out_dir = args.iter().find_map(|a| a.strip_prefix("--out-dir=").map(String::from))
        .unwrap_or_else(|| "samples".to_string());

    let speech = load_wav(&input);
    println!("input: {:.1} s @ 48 kHz mono i16 ({input})", speech.len() as f64 / RATE as f64);

    let stem = input.rsplit('/').next().unwrap_or(&input)
        .split('.').next().unwrap_or(&input);

    let do_s = variant == "s" || variant == "both";
    let do_b = variant == "b" || variant == "both";
    let do_m = variant == "m" || variant == "both";
    let do_ns = variant == "ns";

    if do_ns {
        // APM-only chain (HPF + NS VeryHigh + GC2) + limiter — the app's
        // default chain when no external denoiser is configured.
        run_variant("NS-only (APM VeryHigh + GC2, sin FE)", None, &speech,
                    &format!("{out_dir}/{stem}_ns.wav"));
    }
    if do_s {
        let mut fe = FeEngine::new(fe_s_init, fe_s_run, fe_s_free, WEIGHTS_S, 512)
            .expect("fe_s init failed (needs AVX2+FMA3+F16C)");
        run_variant("FastEnhancer-Small 48k (int8, hop 512)", Some(&mut fe), &speech,
                    &format!("{out_dir}/{stem}_s.wav"));
        unsafe { fe_s_fe_profile_dump(fe.fe_calls as std::os::raw::c_int) };
    }
    if do_m {
        let mut fe = FeEngine::new(fe_m_init, fe_m_run, fe_m_free, WEIGHTS_M, 320)
            .expect("fe_m init failed (needs AVX2+FMA3+F16C)");
        run_variant("FastEnhancer-Medium 48k (int8, hop 320)", Some(&mut fe), &speech,
                    &format!("{out_dir}/{stem}_m.wav"));
        unsafe { fe_m_fe_profile_dump(fe.fe_calls as std::os::raw::c_int) };
    }
    if do_b {
        let rc = unsafe { fe_b_init(
            WEIGHTS_B.as_ptr() as *const std::os::raw::c_void,
            WEIGHTS_B.len() as std::os::raw::c_int,
        ) };
        eprintln!("fe_b_init -> {rc}");
        if rc != 0 {
            std::process::exit(3);
        }
        let mut fe = FeEngine::from_init(fe_b_run, fe_b_free, 512);
        run_variant("FastEnhancer-Base 48k (int8, hop 512, scalar tail)", Some(&mut fe), &speech,
                    &format!("{out_dir}/{stem}_b.wav"));
        unsafe { fe_b_fe_profile_dump(fe.fe_calls as std::os::raw::c_int) };
    }
    println!("done");
}
