//! Voice pipeline harness: process the ORIGINAL sample through the pipeline
//! stage by stage and dump every stage to WAV + per-frame metrics, so the
//! stage interactions and tunings can be analyzed WITHOUT recompiling.
//!
//! Stages:
//!   0 raw capture (written for reference)
//!   1 WebRTC APM (AEC3 transparent-patched + HPF + limiter), far-end fed
//!   2 + DeepFilterNet
//!   3 + leveler (REPLICATED in this harness, env-tunable)
//!   4 + limit_peaks
//!
//! Env knobs (no recompile):
//!   HARNESS_MODE=full (default) | leveler   — leveler re-runs the saved
//!     stage-2 output with new leveler params in ~1 s.
//!   HARNESS_NOISE=<rms>                    — noise added to the capture
//!     (default 0 — the voice-quality focus).
//!   HARNESS_L_TARGET/_MAX/_MIN/_FLOOR/_ATTACK/_RELEASE/_HANGOVER/_STEP/_DECAY
//!
//! Outputs: samples/harness_stage{0..4}.wav, samples/harness_frames.csv,
//! samples/harness_leveler.csv
//!
//! Run: cargo test -p lumen-voice --test voice_harness -- --ignored --nocapture

use lumen_voice::audio::{limit_peaks, rms_level, CLOCK_RATE, DeepFilterDenoiser};
use std::io::Read;

const RATE: u32 = 48_000;
const FRAME: usize = 960;
const OUT: &str = "../../samples";

fn env_f32(name: &str, default: f32) -> f32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn env_u32(name: &str, default: u32) -> u32 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn load(path: &str) -> Vec<i16> {
    let mut file = std::fs::File::open(std::env::current_dir().unwrap().join(path)).unwrap();
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).unwrap();
    buf[44..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

fn write_wav(path: &str, pcm: &[i16]) {
    let mut buf = Vec::with_capacity(44 + pcm.len() * 2);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + pcm.len() as u32 * 2).to_le_bytes());
    buf.extend_from_slice(b"WAVEfmt ");
    buf.extend_from_slice(&16u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&RATE.to_le_bytes());
    buf.extend_from_slice(&(RATE * 2).to_le_bytes());
    buf.extend_from_slice(&2u16.to_le_bytes());
    buf.extend_from_slice(&16u16.to_le_bytes());
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(pcm.len() as u32 * 2).to_le_bytes());
    for &s in pcm {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, &buf).unwrap();
}

fn scale(v: &[i16], g: f32) -> Vec<i16> {
    v.iter().map(|&s| ((s as f32) * g).round() as i16).collect()
}

fn mix(a: &[i16], b: &[i16]) -> Vec<i16> {
    a.iter().zip(b).map(|(&x, &y)| (x as i32 + y as i32).clamp(i16::MIN as i32, i16::MAX as i32) as i16).collect()
}

fn noise(n: usize) -> Vec<i16> {
    let mut state = 0x1234_5678u32;
    (0..n).map(|_| { state ^= state << 13; state ^= state >> 17; state ^= state << 5; ((state >> 8) as i16) / 4 }).collect()
}

/// The leveler REPLICA — same algorithm as `SpeechLeveler`, env-tunable so
/// configs can be iterated without recompiling. Defaults = production values.
struct Lvl {
    target_rms: f32,
    max_gain: f32,
    min_gain: f32,
    floor: f32,
    attack: f32,
    release: f32,
    hangover: u32,
    max_step: f32,
    decay: f32,
    gain_up: f32,
    gain: f32,
    vad_smooth: f32,
    speech_rms: f32,
    hold: u32,
}

impl Lvl {
    fn from_env() -> Self {
        Self {
            target_rms: env_f32("HARNESS_L_TARGET", 0.12),
            max_gain: env_f32("HARNESS_L_MAX", 8.0),
            min_gain: env_f32("HARNESS_L_MIN", 0.25),
            floor: env_f32("HARNESS_L_FLOOR", 0.02),
            attack: env_f32("HARNESS_L_ATTACK", 0.95),
            release: env_f32("HARNESS_L_RELEASE", 0.98),
            hangover: env_u32("HARNESS_L_HANGOVER", 50),
            max_step: env_f32("HARNESS_L_STEP", 0.3),
            decay: env_f32("HARNESS_L_DECAY", 0.85),
            gain_up: env_f32("HARNESS_L_GAIN_UP", 0.1),
            gain: 1.0,
            vad_smooth: 0.0,
            speech_rms: 0.05,
            hold: 0,
        }
    }
    fn process(&mut self, vad: f32, frame_rms: f32, samples: &mut [i16]) {
        self.vad_smooth = self.vad_smooth * 0.8 + vad * 0.2;
        let speech = self.vad_smooth > 0.5 || frame_rms > self.floor;
        if speech {
            if frame_rms > self.speech_rms {
                self.speech_rms = self.speech_rms * self.attack + frame_rms * (1.0 - self.attack);
            } else {
                self.speech_rms = self.speech_rms * self.release + frame_rms * (1.0 - self.release);
            }
            let desired =
                (self.target_rms / self.speech_rms.max(0.0001)).clamp(self.min_gain, self.max_gain);
            let new_gain = self.gain * (1.0 - self.gain_up) + desired * self.gain_up;
            let delta = new_gain - self.gain;
            self.gain = if delta.abs() > self.max_step {
                self.gain + self.max_step.copysign(delta)
            } else {
                new_gain
            };
            self.hold = self.hangover;
        } else if self.hold > 0 {
            self.hold -= 1;
        } else {
            self.gain = (self.gain - 1.0) * self.decay + 1.0;
        }
        if (self.gain - 1.0).abs() > 0.0001 {
            for s in samples.iter_mut() {
                *s = ((*s as f32) * self.gain)
                    .round()
                    .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            }
        }
    }
}

#[test]
#[ignore = "manual harness: dumps WAVs + CSVs to samples/"]
fn voice_harness() {
    let mode = std::env::var("HARNESS_MODE").unwrap_or_else(|_| "full".into());
    if mode == "leveler" {
        run_leveler_only();
        return;
    }

    let mut speech = load("../../samples/real_speech_es_48k.wav");
    speech.truncate(speech.len() - speech.len() % FRAME);
    let n_frames = speech.len() / FRAME;

    // Far-end: non-periodic synthetic speech (AM harmonics), active from t=0.
    let render: Vec<i16> = (0..speech.len())
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

    // Capture: original speech (+ optional echo/noise). HARNESS_VOICE scales
    // the speech (e.g. 0.5 = the quiet-mic hard case the probes use).
    let voice_gain = env_f32("HARNESS_VOICE", 1.0);
    let mut speech = scale(&speech, voice_gain);
    let mut echo = vec![0i16; 2400];
    echo.extend(scale(&render[..render.len() - 2400], 0.4));
    let mut capture = mix(&speech, &echo);
    let noise_rms = env_f32("HARNESS_NOISE", 0.0);
    if noise_rms > 0.0 {
        let raw = noise(speech.len());
        let g = noise_rms / rms_level(&raw);
        capture = mix(&capture, &scale(&raw, g));
    }
    write_wav(&format!("{OUT}/harness_stage0.wav"), &capture);

    // Stage 1: APM (AEC3 + HPF + limiter), render fed.
    use webrtc_audio_processing::config::{
        Config, EchoCanceller, GainController, GainController1, GainControllerMode, HighPassFilter,
    };
    use webrtc_audio_processing::Processor;
    let init_state = env_f32("HARNESS_AEC_INITIAL_STATE", 2.5);
    let aec_off = std::env::var("HARNESS_AEC_NONE").is_ok();
    let hpf_off = std::env::var("HARNESS_HPF_OFF").is_ok();
    let limiter_off = std::env::var("HARNESS_LIMITER_OFF").is_ok();
    let ns_on = std::env::var("HARNESS_NS").is_ok();
    let ns_level = std::env::var("HARNESS_NS_LEVEL").unwrap_or_else(|_| "veryhigh".into());
    let denoiser_none = std::env::var("HARNESS_DENOISER_NONE").is_ok();
    let leveler_off = std::env::var("HARNESS_LEVELER_OFF").is_ok();
    let light = std::env::var("HARNESS_LIGHT").is_ok();
    let gc2 = std::env::var("HARNESS_GC2").is_ok();
    let apm = if init_state < 2.5 && !aec_off {
        // Shorter transparent window: the patch's G=1.0 lasts
        // initial_state_seconds — 2.5 s of echo leak per reset is too long
        // (the leak contaminates the denoiser input and drops its LSNR).
        use webrtc_audio_processing::experimental::EchoCanceller3Config;
        let mut cfg = EchoCanceller3Config::default();
        cfg.filter.initial_state_seconds = init_state;
        assert!(cfg.validate());
        eprintln!("AEC3 initial_state_seconds = {init_state}");
        Processor::with_aec3_config(CLOCK_RATE, cfg).expect("APM init")
    } else {
        eprintln!("AEC3 initial_state_seconds = 2.5 (default)");
        Processor::new(CLOCK_RATE).expect("APM init")
    };
    eprintln!("isolation: aec_off={aec_off} hpf_off={hpf_off} limiter_off={limiter_off} ns_on={ns_on}");
    use webrtc_audio_processing::config::NoiseSuppression as NsCfg;
    use webrtc_audio_processing::config::NoiseSuppressionLevel;
    use webrtc_audio_processing::config::{
        AdaptiveDigital, FixedDigital, GainController2,
    };
    let gain_controller = if gc2 {
        // The reference AGC (Chrome/Meet/Discord): continuous adaptation at
        // max_gain_change_db_per_second (no per-phrase ramp -> no volume
        // surge), noise-capped at max_output_noise_level_dbfs.
        Some(GainController::GainController2(GainController2 {
            input_volume_controller_enabled: false,
            adaptive_digital: Some(AdaptiveDigital {
                headroom_db: env_f32("HARNESS_GC2_HEADROOM", 5.0),
                max_gain_db: env_f32("HARNESS_GC2_MAX", 50.0),
                initial_gain_db: env_f32("HARNESS_GC2_INITIAL", 15.0),
                max_gain_change_db_per_second: env_f32("HARNESS_GC2_SPEED", 6.0),
                max_output_noise_level_dbfs: env_f32("HARNESS_GC2_NOISE_CAP", -50.0),
            }),
            fixed_digital: FixedDigital { gain_db: 0.0 },
        }))
    } else if limiter_off {
        None
    } else {
        Some(GainController::GainController1(GainController1 {
            mode: GainControllerMode::FixedDigital,
            target_level_dbfs: 1,
            compression_gain_db: 0,
            enable_limiter: true,
            analog_gain_controller: None,
        }))
    };
    eprintln!("isolation: aec_off={aec_off} hpf_off={hpf_off} limiter_off={limiter_off} ns_on={ns_on} gc2={gc2}");
    apm.set_config(Config {
        echo_canceller: if aec_off {
            None
        } else {
            Some(EchoCanceller::Full { stream_delay_ms: None })
        },
        high_pass_filter: if hpf_off {
            None
        } else {
            Some(HighPassFilter { apply_in_full_band: true })
        },
        noise_suppression: if ns_on {
            Some(NsCfg {
                level: match ns_level.as_str() {
                    "low" => NoiseSuppressionLevel::Low,
                    "moderate" => NoiseSuppressionLevel::Moderate,
                    "high" => NoiseSuppressionLevel::High,
                    _ => NoiseSuppressionLevel::VeryHigh,
                },
                analyze_linear_aec_output: false,
            })
        } else {
            None
        },
        gain_controller,
        ..Config::default()
    });
    let mut stage1 = Vec::with_capacity(speech.len());
    for (i, f) in capture.chunks_exact(FRAME).enumerate() {
        for c in render[i * FRAME..(i + 1) * FRAME].chunks(480) {
            let mut buf = [0f32; 480];
            for (j, s) in c.iter().enumerate() {
                buf[j] = *s as f32 / 32768.0;
            }
            apm.process_render_frame([&mut buf]).ok();
        }
        let mut out = vec![0i16; FRAME];
        for (k, chunk) in f.chunks_exact(480).enumerate() {
            let mut buf = [0f32; 480];
            for (j, s) in chunk.iter().enumerate() {
                buf[j] = *s as f32 / 32768.0;
            }
            apm.process_capture_frame([&mut buf]).ok();
            for (j, v) in buf.iter().enumerate() {
                out[k * 480 + j] =
                    (v * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            }
        }
        stage1.extend_from_slice(&out);
    }
    write_wav(&format!("{OUT}/harness_stage1.wav"), &stage1);

    // Stage 2: DeepFilterNet (or passthrough / RNNoise for the other chains).
    let rnnoise_on = std::env::var("HARNESS_RNNOISE").is_ok();
    let mut stage2 = Vec::with_capacity(speech.len());
    let mut lsnrs = Vec::with_capacity(n_frames);
    if denoiser_none {
        stage2 = stage1.clone();
        lsnrs = vec![30.0; n_frames]; // clean speech VAD
    } else if rnnoise_on {
        let mut rn = lumen_voice::audio::RnnoiseDenoiser::new();
        for f in stage1.chunks_exact(FRAME) {
            let (denoised, _vad) = rn.process(f);
            stage2.extend_from_slice(&denoised);
            lsnrs.push(30.0); // RNNoise VAD is its own signal; use clean VAD
        }
    } else {
        let mut model = DeepFilterDenoiser::new().expect("model");
        for f in stage1.chunks_exact(FRAME) {
            let (denoised, lsnr) = model.process(f);
            stage2.extend_from_slice(&denoised);
            lsnrs.push(lsnr);
        }
    }
    write_wav(&format!("{OUT}/harness_stage2.wav"), &stage2);

    // Stage 3: leveler replica + Stage 4: limiter, with per-frame CSV.
    let blend = env_f32("HARNESS_BLEND", 0.0); // dry-wet: blend of denoised with post-AEC
    let mut lvl = Lvl::from_env();
    let mut stage3 = Vec::with_capacity(stage2.len());
    let mut csv = String::from("frame,in_rms,apm_rms,denoised_rms,lsnr,gain,out_rms\n");
    for (i, f) in stage2.chunks_exact(FRAME).enumerate() {
        let mut out = f.to_vec();
        if blend > 0.0 {
            let a = blend;
            for (o, &s1) in out.iter_mut().zip(stage1[i * FRAME..(i + 1) * FRAME].iter()) {
                *o = ((*o as f32) * a + s1 as f32 * (1.0 - a))
                    .round()
                    .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            }
        }
        if !leveler_off {
            let lsnr = lsnrs[i];
            let vad = ((lsnr - (-10.0)) / 40.0).clamp(0.0, 1.0);
            lvl.process(vad, rms_level(&out), &mut out);
        }
        limit_peaks(&mut out, 1.0);
        stage3.extend_from_slice(&out);
        csv.push_str(&format!(
            "{i},{:.6},{:.6},{:.6},{:.1},{:.4},{:.6}\n",
            rms_level(&capture[i * FRAME..(i + 1) * FRAME]),
            rms_level(&stage1[i * FRAME..(i + 1) * FRAME]),
            rms_level(f),
            lsnrs[i],
            lvl.gain,
            rms_level(&out),
        ));
    }
    write_wav(&format!("{OUT}/harness_stage3.wav"), &stage3);
    std::fs::write(format!("{OUT}/harness_frames.csv"), csv).unwrap();

    // Chain C: the light (RNNoise) reference chain — AEC + NS + RNNoise + leveler.
    if light {
        let mut ns = lumen_voice::audio::NoiseSuppressor::new_light();
        let mut out_all = Vec::with_capacity(capture.len());
        for (i, f) in capture.chunks_exact(FRAME).enumerate() {
            for c in render[i * FRAME..(i + 1) * FRAME].chunks(480) {
                ns.process_render_frame(c);
            }
            out_all.extend_from_slice(&ns.process(f));
        }
        write_wav(&format!("{OUT}/harness_light.wav"), &out_all);
        println!("light: wrote {OUT}/harness_light.wav");
    }
    println!("full: stage0-3 written to {OUT}/harness_stage*.wav + harness_frames.csv");
}

fn run_leveler_only() {
    let stage2 = load(&format!("{OUT}/harness_stage2.wav"));
    let lsnr_csv = std::fs::read_to_string(format!("{OUT}/harness_frames.csv")).unwrap();
    let lsnrs: Vec<f32> = lsnr_csv
        .lines()
        .skip(1)
        .map(|l| l.split(',').nth(4).unwrap().parse().unwrap())
        .collect();
    let mut lvl = Lvl::from_env();
    let mut out_all = Vec::with_capacity(stage2.len());
    let mut csv = String::from("frame,lsnr,gain,out_rms\n");
    for (i, f) in stage2.chunks_exact(FRAME).enumerate() {
        let mut out = f.to_vec();
        let lsnr = lsnrs[i];
        let vad = ((lsnr - (-10.0)) / 40.0).clamp(0.0, 1.0);
        lvl.process(vad, rms_level(&out), &mut out);
        limit_peaks(&mut out, 1.0);
        out_all.extend_from_slice(&out);
        csv.push_str(&format!("{i},{lsnr:.1},{:.4},{:.6}\n", lvl.gain, rms_level(&out)));
    }
    write_wav(&format!("{OUT}/harness_stage3.wav"), &out_all);
    std::fs::write(format!("{OUT}/harness_leveler.csv"), csv).unwrap();
    println!("leveler: re-applied with HARNESS_L_* params -> harness_stage3.wav + harness_leveler.csv");
}
