//! Release RTF benchmark: NS-only chain vs DF3 chain vs (optionally) DPDFNet,
//! over the same 36 s hard input. Run with `--release` (tract/APM debug is
//! 6x slower — meaningless).
//!
//! Run: `cargo test --release -p lumen-voice --test rtf_bench -- --ignored --nocapture`

use lumen_voice::audio::{rms_level, NoiseSuppressor};
use std::io::Read;
use std::time::Instant;

const RATE: u32 = 48_000;
const FRAME: usize = 960;

fn load(path: &str) -> Vec<i16> {
    let mut f = std::fs::File::open(std::env::current_dir().unwrap().join(path)).unwrap();
    let mut b = Vec::new();
    f.read_to_end(&mut b).unwrap();
    b[44..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect()
}

fn scale(v: &[i16], g: f32) -> Vec<i16> {
    v.iter().map(|s| ((*s as f32) * g).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16).collect()
}

fn mix(a: &[i16], b: &[i16]) -> Vec<i16> {
    a.iter().zip(b.iter()).map(|(x, y)| x.saturating_add(*y)).collect()
}

#[test]
#[ignore = "manual RTF benchmark (release build)"]
fn rtf_bench() {
    let mut speech = load("../../samples/real_speech_es_48k.wav");
    speech.drain(..5 * RATE as usize);
    speech.truncate(speech.len() - speech.len() % FRAME);
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
    let noise_raw: Vec<i16> = (0..speech.len()).map(|_| {
        state ^= state << 13; state ^= state >> 17; state ^= state << 5;
        ((state >> 8) as i16) / 4
    }).collect();
    let noise = scale(&noise_raw, 0.01 / rms_level(&noise_raw));
    let capture = mix(&mix(&quiet, &noise), &echo);
    let audio_s = capture.len() as f64 / RATE as f64;

    fn bench(ns: &mut NoiseSuppressor, capture: &[i16], render: &[i16], audio_s: f64, label: &str) {
        let t0 = Instant::now();
        let mut sink = 0i32;
        for (i, f) in capture.chunks_exact(FRAME).enumerate() {
            ns.process_render_frame(&render[i * FRAME..(i + 1) * FRAME]);
            let out = ns.process(f);
            sink = sink.wrapping_add(out.iter().map(|s| *s as i32).sum::<i32>());
        }
        let secs = t0.elapsed().as_secs_f64();
        println!("{label:22} RTF {:.3}  ({} s audio, {:.2} s CPU, sink {sink})",
            secs / audio_s, audio_s, secs);
    }

    println!("=== release RTF (hard input, {audio_s:.0} s) ===");
    let mut ns = NoiseSuppressor::new();
    bench(&mut ns, &capture, &render, audio_s, "NS-only (AEC+NS+GC2)");
    drop(ns);
    let mut nn = NoiseSuppressor::new_neural();
    bench(&mut nn, &capture, &render, audio_s, "DF3 (new_neural)");
}
