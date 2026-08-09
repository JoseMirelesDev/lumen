//! DPDFNet 48 kHz probe — the full-band denoiser candidate (dpdfnet2_48khz_hr,
//! Apache-2.0, sherpa-onnx release). Runs via tract (already in the tree via
//! deep_filter) with the exact sherpa-onnx I/O contract:
//!   STFT n_fft=960, hop=480, Vorbis window, center+reflect pad, NOT
//!   normalized; per-frame model input [1,1,481,2] real/imag interleaved +
//!   RNN state (state_size floats); output enhanced STFT + next state;
//!   ISTFT + shift by window_length*2.
//!
//! Outputs: samples/harness_dpdfnet_hard36.wav (hard input, 48 kHz native)
//! Prints floor / voice / pumping / RTF.
//!
//! Run: `cargo test -p lumen-voice --test dpdfnet_probe -- --ignored --nocapture`

use ndarray::Array2;
use rustfft::{num_complex::Complex, FftPlanner};
use std::time::Instant;

const RATE: u32 = 48_000;
const N_FFT: usize = 960;
const HOP: usize = 480;
const STATE_SIZE: usize = 56_436;
const MODEL: &str = "../../samples/models/dpdfnet2_48khz_hr.onnx";

fn load(path: &str) -> Vec<i16> {
    use std::io::Read;
    let mut f = std::fs::File::open(std::env::current_dir().unwrap().join(path)).unwrap();
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

/// Vorbis window (Kaldi convention): sin(pi/2 * sin(pi*n/(N-1))^2).
fn vorbis_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let a = (std::f32::consts::PI * i as f32 / (n - 1) as f32).sin();
            (std::f32::consts::FRAC_PI_2 * a * a).sin()
        })
        .collect()
}

#[test]
#[ignore = "manual probe: DPDFNet 48k on the hard input"]
fn dpdfnet_probe() {
    // ---- input: the hard 36 s scenario (quiet voice + noise + echo) ----
    let mut speech = load("../../samples/real_speech_es_48k.wav");
    speech.drain(..5 * RATE as usize);
    speech.truncate(speech.len() - speech.len() % (2 * HOP));
    let scale = |v: &[i16], g: f32| -> Vec<i16> {
        v.iter()
            .map(|s| ((*s as f32) * g).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
            .collect()
    };
    let mix = |a: &[i16], b: &[i16]| -> Vec<i16> {
        a.iter().zip(b.iter()).map(|(x, y)| x.saturating_add(*y)).collect()
    };
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
    let mut echo = vec![0i16; 2400];
    echo.extend(scale(&render[..render.len() - 2400], 0.4));
    let quiet = scale(&speech, 0.5);
    let mut state = 0x1234_5678u32;
    let noise_raw: Vec<i16> = (0..speech.len())
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            ((state >> 8) as i16) / 4
        })
        .collect();
    let noise = scale(&noise_raw, 0.01 / lumen_voice::audio::rms_level(&noise_raw));
    let capture = mix(&mix(&quiet, &noise), &echo);
    println!("=== DPDFNet 48 kHz probe ===");
    println!("input: {:.1} s @ 48 kHz (hard: quiet voice + noise + echo)",
        capture.len() as f64 / RATE as f64);

    // ---- model ----
    use tract_onnx::prelude::*;
    let model = tract_onnx::onnx()
        .model_for_path(MODEL)
        .expect("model load")
        .into_optimized()
        .expect("optimize")
        .into_runnable()
        .expect("runnable");
    let state = ndarray::Array::<f32, _>::zeros([STATE_SIZE]);

    // ---- STFT ----
    let win = vorbis_window(N_FFT);
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(N_FFT);
    let ifft = planner.plan_fft_inverse(N_FFT);

    // center=true + reflect padding (pad_mode=reflect): 480 each side.
    let mut padded = vec![0f32; capture.len() + N_FFT];
    for i in 0..N_FFT / 2 {
        padded[N_FFT / 2 - 1 - i] = capture[i] as f32 / 32768.0;
        padded[N_FFT / 2 + capture.len() + i] =
            capture[capture.len() - 1 - i] as f32 / 32768.0;
    }
    for (i, &s) in capture.iter().enumerate() {
        padded[N_FFT / 2 + i] = s as f32 / 32768.0;
    }
    let n_frames = (padded.len() - N_FFT) / HOP + 1;

    let mut enhanced_istft = vec![0f32; padded.len() + N_FFT];
    let mut next_state = state.clone();
    let t0 = Instant::now();
    for f in 0..n_frames {
        let start = f * HOP;
        // analysis: windowed frame -> FFT -> real/imag interleaved
        let mut buf = [Complex::new(0f32, 0f32); N_FFT];
        for (i, c) in buf.iter_mut().enumerate() {
            *c = Complex::new(padded[start + i] * win[i], 0.0);
        }
        fft.process(&mut buf);
        let mut x = vec![0f32; (N_FFT / 2 + 1) * 2];
        for i in 0..N_FFT / 2 + 1 {
            x[2 * i] = buf[i].re;
            x[2 * i + 1] = buf[i].im;
        }
        let x_t = ndarray::Array::from_shape_vec([1, 1, N_FFT / 2 + 1, 2], x).unwrap();
        let outs = model
            .run(tvec!(x_t.into_tensor().into(), next_state.clone().into_tensor().into()))
            .expect("model run");
        // outs[0] = enhanced STFT [1,1,481,2], outs[1] = next state
        let enh = outs[0].to_array_view::<f32>().unwrap();
        let st = outs[1].to_array_view::<f32>().unwrap();
        next_state = st.to_owned().into_shape([STATE_SIZE]).unwrap();
        // synthesis: inverse FFT of the enhanced spectrum + overlap-add
        let mut buf2 = [Complex::new(0f32, 0f32); N_FFT];
        for i in 0..N_FFT / 2 + 1 {
            buf2[i] = Complex::new(enh[[0, 0, i, 0]], enh[[0, 0, i, 1]]);
        }
        for i in 1..N_FFT / 2 {
            buf2[N_FFT - i] = buf2[i].conj();
        }
        ifft.process(&mut buf2);
        let inv = 1.0 / N_FFT as f32;
        for i in 0..N_FFT {
            enhanced_istft[start + i] += buf2[i].re * inv * win[i];
        }
    }
    let proc_s = t0.elapsed().as_secs_f64();
    let audio_s = capture.len() as f64 / RATE as f64;
    println!("RTF (debug build): {:.3}  ({} s audio in {:.2} s)",
        proc_s / audio_s, audio_s, proc_s);

    // ---- delay compensation: shift by window_length*2, truncate ----
    let shift = N_FFT * 2;
    let mut out: Vec<i16> = Vec::with_capacity(capture.len());
    for i in shift..shift + capture.len() {
        out.push((enhanced_istft[i] * 32767.0).round().clamp(-32768.0, 32767.0) as i16);
    }
    write_wav("../../samples/harness_dpdfnet_hard36.wav", &out);

    // ---- metrics ----
    let rms = |x: &[i16]| -> f32 {
        (x.iter().map(|&s| (s as i64) * (s as i64)).sum::<i64>() as f64 / x.len() as f64)
            .sqrt() as f32
    };
    let win = 4800usize;
    let mut owins: Vec<f32> = out.chunks(win).map(rms).collect();
    owins.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let floor = owins[owins.len() / 10];
    let voice = owins[(owins.len() as f32 * 0.9) as usize];
    // pumping: p90 of consecutive-frame deltas during speech
    let frames: Vec<f32> = out.chunks(960).map(rms).collect();
    let mut deltas: Vec<f32> = frames
        .windows(2)
        .filter(|w| w[0] > 0.02 && w[1] > 0.02)
        .map(|w| (w[1] - w[0]).abs() / w[0].max(1e-6))
        .collect();
    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pump = deltas[(deltas.len() as f32 * 0.9) as usize];
    println!("floor {:.1} dBFS | voice p90 {:.1} dBFS | pumping {:.3}",
        20.0 * (floor / 32768.0).log10(), 20.0 * (voice / 32768.0).log10(), pump);
    println!("wrote samples/harness_dpdfnet_hard36.wav");
    println!("=== end probe ===");
    let _ = Array2::<f32>::default((1, 1)); // ndarray referenced
}
