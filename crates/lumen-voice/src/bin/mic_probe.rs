//! Live mic probe: runs the real app send pipeline (WebRTC APM — AEC3/HPF/NS —
//! + the selected denoiser tier + GainController2 + limiter) on the live
//! microphone and plays the result to the default output (headphones), with a
//! terminal level meter + VAD indicator.
//!
//! Usage:
//!   cargo run -p lumen-voice --bin mic_probe [fastenhancer|ns-only]
//!
//! Default model is FastEnhancer-M (auto-degrades to NS-only if the CPU can't
//! run it). Ctrl+C to stop.

use std::env;

use lumen_voice::audio::{
    self, rms_level, AudioOutput, NoiseSuppressor, SuppressorModel,
};

fn meter(lvl: f32, width: usize) -> String {
    let filled = ((lvl.clamp(0.0, 1.0) * width as f32).round() as usize).min(width);
    format!("{}{}", "#".repeat(filled), "-".repeat(width - filled))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let model = match env::args().nth(1).as_deref() {
        Some("ns-only") => SuppressorModel::NsOnly,
        Some("fastenhancer") => SuppressorModel::FastEnhancerM,
        Some(other) => {
            eprintln!("unknown model '{other}' — use fastenhancer | ns-only");
            std::process::exit(2);
        }
        None => SuppressorModel::FastEnhancerM,
    };
    let ns = NoiseSuppressor::with_model(model);
    println!(
        "mic probe: model={} neural_loaded={} ({} Hz, {} samples/frame)",
        model.as_str(),
        ns.neural_available(),
        audio::CLOCK_RATE,
        audio::FRAME_SAMPLES,
    );

    let (mic_tx, mut mic_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<i16>>();
    let _mic = audio::start_capture(mic_tx)?;
    let mut out = AudioOutput::new();
    // The cpal::Stream must be held alive — dropping it stops playback.
    let _out_stream = out.start()?;

    let mut ns = ns;
    let mut frames = 0usize;
    loop {
        let Some(frame) = mic_rx.recv().await else { break };
        let processed = ns.process(&frame);
        out.push(&processed);
        frames += 1;
        if frames % 25 == 0 {
            // 25 frames = 0.5 s of audio.
            let in_lvl = rms_level(&frame);
            let out_lvl = rms_level(&processed);
            let vad = if ns.speech_detected() { "VAD " } else { "    " };
            println!(
                "{vad} in [{:>3.0}%] {:<40} out [{:>3.0}%] {:<40}",
                in_lvl * 100.0,
                meter(in_lvl * 1.6, 40),
                out_lvl * 100.0,
                meter(out_lvl * 1.6, 40),
            );
        }
    }
    Ok(())
}
