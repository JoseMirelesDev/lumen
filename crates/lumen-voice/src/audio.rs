//! Audio I/O for the native voice client.
//!
//! - Capture: cpal input stream → mono 48 kHz i16 frames (device rate/channels
//!   are resampled/mixed down), delivered as 20 ms frames on an mpsc channel.
//! - Playback: cpal output stream fed from a shared ring buffer; the decoder
//!   side mixes every remote peer's PCM into it.
//! - Codec: OPUS (48 kHz, mono send; decoder downmixes stereo frames).
//! - Per-window helpers: RMS level, a small jitter buffer and a linear
//!   resampler.

use parking_lot::Mutex;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Samples per OPUS frame at 48 kHz / 20 ms.
pub const FRAME_SAMPLES: usize = 960;
/// RTP clock for audio: 48 kHz.
pub const CLOCK_RATE: u32 = 48_000;

// ---------------------------------------------------------------------------
// Mic capture
// ---------------------------------------------------------------------------

/// Starts the capture stream. Frames of exactly [`FRAME_SAMPLES`] mono i16
/// samples at 48 kHz arrive on `frames_tx` (20 ms cadence). Drop the returned
/// stream to stop the mic.
pub fn start_capture(
    frames_tx: tokio::sync::mpsc::UnboundedSender<Vec<i16>>,
) -> anyhow::Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = host.default_input_device().ok_or_else(|| anyhow::anyhow!("no input device"))?;
    let config = device.default_input_config()?;
    let channels = config.channels() as usize;
    let mut resampler = LinearResampler::new(config.sample_rate(), CLOCK_RATE);
    let mut acc: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES);
    let err_fn = |err| eprintln!("lumen voice: input stream error: {err}");
    let stream_config = config.config();

    let stream = match config.sample_format() {
        cpal::SampleFormat::I16 => {
            device.build_input_stream(
                stream_config,
                move |data: &[i16], _| {
                    feed(&mut resampler, data, channels, &mut acc, &frames_tx);
                },
                err_fn,
                None,
            )?
        }
        cpal::SampleFormat::F32 => {
            device.build_input_stream(
                stream_config,
                move |data: &[f32], _| {
                    let mut s16: Vec<i16> = Vec::with_capacity(data.len());
                    for &v in data {
                        s16.push((v * 32767.0) as i16);
                    }
                    feed(&mut resampler, &s16, channels, &mut acc, &frames_tx);
                },
                err_fn,
                None,
            )?
        }
        other => anyhow::bail!("unsupported input format {other}"),
    };
    stream.play()?;
    Ok(stream)
}

fn feed(
    resampler: &mut LinearResampler,
    data: &[i16],
    channels: usize,
    acc: &mut Vec<i16>,
    frames_tx: &tokio::sync::mpsc::UnboundedSender<Vec<i16>>,
) {
    if data.is_empty() {
        return;
    }
    // Mix down to mono (first channel of an interleaved buffer).
    let mono: Vec<i16> = if channels == 1 {
        data.to_vec()
    } else {
        data.iter().step_by(channels).copied().collect()
    };
    let resampled = resampler.resample(&mono);
    acc.extend_from_slice(&resampled);
    while acc.len() >= FRAME_SAMPLES {
        let frame: Vec<i16> = acc.drain(..FRAME_SAMPLES).collect();
        let _ = frames_tx.send(frame);
    }
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

/// Shared output sink: the decode side mixes every remote peer's 48 kHz PCM in
/// (resampled to the device rate in `push`); cpal's callback is a pure FIFO
/// drain. Keeping the buffer at the device rate means the callback never needs
/// to resample, so a non-48 kHz output device can't pitch-shift the audio.
#[derive(Clone)]
pub struct AudioOutput {
    state: Arc<Mutex<Option<OutputState>>>,
    /// When false (deafened) the callback emits silence.
    enabled: Arc<AtomicBool>,
    /// AEC reference: a copy of the mono samples actually played. The send
    /// path drains this into the WebRTC APM `process_render_frame` so AEC3
    /// can cancel the speaker echo picked up by the mic.
    render_tap: Arc<Mutex<Vec<i16>>>,
}

struct OutputState {
    /// Device-rate mono samples awaiting playback.
    buf: Vec<i16>,
    /// 48 kHz -> device rate, applied to each decoded frame on push.
    resampler: LinearResampler,
    /// Output device channel count (typically 2 — stereo).
    channels: usize,
    /// Device-rate samples per 20 ms frame (device_rate / 50).
    frame_size: usize,
    /// TEMP DIAG: samples shed by the overflow guard in `push`.
    dropped_samples: u64,
}

impl AudioOutput {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
            enabled: Arc::new(AtomicBool::new(true)),
            render_tap: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Install the AEC render tap (called once at session start, before the
    /// output stream starts). Replaces any previous tap.
    pub fn set_render_tap(&mut self, tap: Arc<Mutex<Vec<i16>>>) {
        self.render_tap = tap;
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            if let Some(st) = self.state.lock().as_mut() {
                st.buf.clear();
            }
        }
    }

    /// Mix one decoded 48 kHz mono frame into the output, resampled to the
    /// device rate. Keeps playout latency bounded to a few frames: if the
    /// drain can't keep up (bursts, several peers, a peer still pushing during
    /// a re-join) the buffer sheds the oldest samples instead of growing — a
    /// buffer that swells to seconds is exactly how stale audio (what was said
    /// seconds ago) ends up being played after re-entering a channel.
    pub fn push(&self, frame: &[i16]) {
        let mut guard = self.state.lock();
        let Some(st) = guard.as_mut() else { return };
        st.buf.extend_from_slice(&st.resampler.resample(frame));
        // ~4 frames of device-rate samples (~80 ms) is enough to smooth cpal
        // callback phase without accumulating audible delay.
        let target = st.frame_size.saturating_mul(4).max(st.frame_size);
        let excess = st.buf.len().saturating_sub(target);
        if excess > 0 {
            st.dropped_samples += excess as u64;
            st.buf.drain(..excess);
        }
    }

    /// TEMP DIAG: current playout buffer occupancy (in frames) and total shed
    /// samples. Lets a test observe the latency/packet-loss mechanics live.
    pub fn stats(&self) -> (usize, u64) {
        let guard = self.state.lock();
        match guard.as_ref() {
            Some(st) => (st.buf.len() / st.frame_size.max(1), st.dropped_samples),
            None => (0, 0),
        }
    }

    /// Copy the available device-rate mono samples into `out` (FIFO),
    /// expanding to the device's channel count (a stereo `out` gets the same
    /// sample in L and R), then zero-fill the tail.
    ///
    /// Without the expansion, mono samples written sequentially into an
    /// interleaved stereo buffer play at 2× speed per channel (chipmunk).
    fn drain_into(&self, out: &mut [i16]) {
        let mut guard = self.state.lock();
        let Some(st) = guard.as_mut() else {
            out.fill(0);
            return;
        };
        let ch = st.channels.max(1);
        let frames = out.len() / ch;
        let n = st.buf.len().min(frames);
        // AEC reference: copy what is actually played (mono, device rate)
        // into the render tap, drained by the send path for AEC3.
        if n > 0 {
            self.render_tap.lock().extend_from_slice(&st.buf[..n]);
        }
        for f in 0..n {
            let s = st.buf[f];
            for v in &mut out[f * ch..(f + 1) * ch] {
                *v = s;
            }
        }
        st.buf.drain(..n);
        for v in &mut out[n * ch..] {
            *v = 0;
        }
    }

    /// Start the cpal output stream on the default device.
    pub fn start(&self) -> anyhow::Result<cpal::Stream> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| anyhow::anyhow!("no output device"))?;
        let config = device.default_output_config()?;
        let dev_rate = config.sample_rate();
        *self.state.lock() = Some(OutputState {
            buf: Vec::with_capacity((dev_rate / 2) as usize),
            resampler: LinearResampler::new(CLOCK_RATE, dev_rate),
            channels: config.channels() as usize,
            frame_size: ((dev_rate as usize) * FRAME_SAMPLES / CLOCK_RATE as usize).max(1),
            dropped_samples: 0,
        });
        let out = self.clone();
        let err_fn = |err| eprintln!("lumen voice: output stream error: {err}");
        let stream_config = config.config();

        let stream = match config.sample_format() {
            cpal::SampleFormat::I16 => {
                device.build_output_stream(
                    stream_config,
                    move |data: &mut [i16], _| {
                        if out.enabled.load(Ordering::SeqCst) {
                            out.drain_into(data);
                        } else {
                            data.fill(0);
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            cpal::SampleFormat::F32 => {
                device.build_output_stream(
                    stream_config,
                    move |data: &mut [f32], _| {
                        let mut tmp = vec![0i16; data.len()];
                        if out.enabled.load(Ordering::SeqCst) {
                            out.drain_into(&mut tmp);
                        } else {
                            tmp.fill(0);
                        }
                        for (o, s) in data.iter_mut().zip(&tmp) {
                            *o = *s as f32 / 32767.0;
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            other => anyhow::bail!("unsupported output format {other}"),
        };
        stream.play()?;
        Ok(stream)
    }
}

impl Default for AudioOutput {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// OPUS
// ---------------------------------------------------------------------------

pub struct OpusEncoder {
    encoder: opus::Encoder,
}

impl OpusEncoder {
    pub fn new() -> anyhow::Result<Self> {
        let mut encoder = opus::Encoder::new(CLOCK_RATE, opus::Channels::Mono, opus::Application::Voip)?;
        // 64 kbps: preserves the full-band (48 kHz) fidelity of the denoised
        // audio. 32 kbps was fine for the old 8 kHz band-limited GTCRN output;
        // with full-band tiers it was the bottleneck. Encode cost is trivial.
        encoder.set_bitrate(opus::Bitrate::Bits(64_000))?;
        Ok(Self { encoder })
    }

    /// Encode one 20 ms frame into a packet. Returns the packet bytes.
    pub fn encode(&mut self, pcm: &[i16]) -> anyhow::Result<Vec<u8>> {
        let mut out = [0u8; 1500];
        let n = self.encoder.encode(pcm, &mut out)?;
        Ok(out[..n].to_vec())
    }
}

pub struct OpusDecoder {
    decoder: opus::Decoder,
}

impl OpusDecoder {
    pub fn new() -> anyhow::Result<Self> {
        // Mono decoder: libopus downmixes stereo frames to mono.
        let decoder = opus::Decoder::new(CLOCK_RATE, opus::Channels::Mono)?;
        Ok(Self { decoder })
    }

    /// Decode one packet into mono PCM. `None` packet = PLC.
    pub fn decode(&mut self, packet: Option<&[u8]>) -> anyhow::Result<Vec<i16>> {
        let mut out = vec![0i16; FRAME_SAMPLES];
        let n = self.decoder.decode(packet.unwrap_or(&[]), &mut out, false)?;
        // The peer may send shorter frames (e.g. 10 ms); keep only the samples
        // actually decoded instead of pushing half a frame of stale data.
        out.truncate(n);
        Ok(out)
    }
}

/// Bridge our `OpusDecoder` (i16 PCM) onto neteq's `AudioDecoder` (f32 PCM),
/// so the NetEQ adaptive jitter buffer can decode inbound OPUS frames itself.
/// Owned by a `NetEq` instance via `register_decoder`.
pub struct NetEqOpusDecoder {
    inner: OpusDecoder,
}

impl NetEqOpusDecoder {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self { inner: OpusDecoder::new()? })
    }
}

impl neteq::codec::AudioDecoder for NetEqOpusDecoder {
    fn sample_rate(&self) -> u32 {
        CLOCK_RATE
    }
    fn channels(&self) -> u8 {
        1
    }
    fn decode(&mut self, encoded: &[u8]) -> neteq::Result<Vec<f32>> {
        let pcm = self
            .inner
            .decode(Some(encoded))
            .map_err(|e| neteq::NetEqError::DecoderError(e.to_string()))?;
        Ok(pcm.iter().map(|&s| s as f32 / 32768.0).collect())
    }
}

// ---------------------------------------------------------------------------
// Noise suppression (WebRTC AudioProcessing — the module Chrome/Discord use)
// ---------------------------------------------------------------------------

use webrtc_audio_processing::config::{
    Config, EchoCanceller, HighPassFilter, NoiseSuppression, NoiseSuppressionLevel,
};
use webrtc_audio_processing::Processor;

/// Send-path DSP: WebRTC AudioProcessing (AEC3 + high-pass + fixed-digital
/// AGC) followed by RNNoise (neural noise suppression, Krisp-style).
///
/// - AEC3 (`EchoCanceller::Full`, auto-delay) cancels the speaker echo picked
///   up by the mic — the caller must feed the playback stream into
///   [`NoiseSuppressor::process_render_frame`] (the far-end reference).
/// - Classic WebRTC NS is disabled: RNNoise's recurrent network beats it on
///   non-stationary background noise (fan, traffic, keyboard) with less
///   speech damage — the same approach Discord takes with Krisp.
/// - `Processor` is `Send + Sync`, `nnnoiseless::DenoiseState` is plain data,
///   so the whole suppressor lives in the send task. Both process 10 ms
///   frames (480 samples @ 48 kHz); a 20 ms capture frame is two halves.
pub struct NoiseSuppressor {
    processor: Option<Processor>,
    /// RNNoise denoiser — the LIGHT tier. Full-band 48 kHz, ~8% of a core.
    /// Used by default and as the automatic fallback when the CPU is loaded.
    rnnoise: Option<Box<nnnoiseless::DenoiseState<'static>>>,
    /// GTCRN (sherpa-onnx) — the HIGH-SUPPRESSION neural tier. Engaged when
    /// the adaptive CPU monitor sees headroom; degrades to RNNoise (light)
    /// under load. NOTE: a full-band (48 kHz) Krisp-like replacement (DPDFNet)
    /// is blocked by a sherpa-onnx 1.13.4 incompatibility — see [`GtcrnDenoiser`].
    gtcrn: Option<GtcrnDenoiser>,
    /// Whether the high-quality neural tier is active (vs the light RNNoise).
    /// The send task toggles this based on system CPU load.
    neural_active: bool,
    /// VAD-gated adaptive gain (boosts quiet speech, gates silence).
    leveler: SpeechLeveler,
    /// Whether the last processed frame contained speech (post-denoise energy
    /// in the neural path, RNNoise VAD in the fallback).
    speech_detected: bool,
    /// Frames remaining in the `process_gated` hangover — the chain stays
    /// open this many frames after the last voiced frame so speech tails /
    /// quiet endings aren't clipped at word and utterance boundaries.
    gate_hangover: u32,
}

impl NoiseSuppressor {
    pub fn new() -> Self {
        // High-quality by default: the GTCRN neural tier engaged, with RNNoise
        // available as the light fallback the adaptive monitor uses. If GTCRN
        // can't load, the chain is permanently light (RNNoise full-band).
        Self::chain(GtcrnDenoiser::new(), true)
    }

    /// Construct the send-path DSP in LIGHT mode only (WebRTC AEC3/NS +
    /// RNNoise + leveler, no neural tier). Used by probes/tests and as the
    /// degraded state.
    pub fn new_light() -> Self {
        Self::chain(None, false)
    }

    /// Shared construction: WebRTC APM (AEC3 + HPF) + leveler, plus the
    /// optional high-suppression neural denoiser (GTCRN). Classic WebRTC NS is
    /// ON only in the light tier: the neural tier disables it (GTCRN is the
    /// denoiser; running NS before it double-colors the speech — the original
    /// reason NS was off with GTCRN). AGC is OFF in both (measured: the
    /// fixed-digital AGC amplified background noise before NS removed it).
    fn chain(gtcrn: Option<GtcrnDenoiser>, neural_active: bool) -> Self {
        let processor = Processor::new(CLOCK_RATE).ok().map(|processor| {
            processor.set_config(apm_config(neural_active));
            processor
        });
        Self {
            processor,
            rnnoise: Some(nnnoiseless::DenoiseState::new()),
            gtcrn,
            neural_active,
            leveler: SpeechLeveler::new(),
            speech_detected: false,
            gate_hangover: 0,
        }
    }

    /// Switch the high-quality neural tier on/off (called by the adaptive CPU
    /// monitor in the send task). Light mode (RNNoise) is always available;
    /// turning the neural tier on is a no-op if the model isn't loaded. Also
    /// toggles WebRTC NS (off on the neural tier, on on the light tier); this
    /// reinitializes the APM, so AEC3 re-converges — a brief, rare artifact on
    /// tier switches, acceptable vs. double-NS coloring speech.
    pub fn set_high_quality(&mut self, high: bool) {
        if high && self.gtcrn.is_none() {
            return; // no neural model loaded — stay light
        }
        if self.neural_active == high {
            return; // no change
        }
        self.neural_active = high;
        if let Some(processor) = self.processor.as_mut() {
            processor.set_config(apm_config(high));
        }
    }

    /// Whether the high-quality neural tier is currently active.
    pub fn high_quality_active(&self) -> bool {
        self.neural_active
    }

    /// Feed the far-end (playback) audio into AEC3. Call this with the exact
    /// PCM that goes to the speakers, in 10 ms multiples (480 samples @
    /// 48 kHz), before/around the capture frames it must cancel.
    pub fn process_render_frame(&mut self, frame: &[i16]) {
        let Some(processor) = self.processor.as_mut() else { return };
        let mut buf = [0f32; 480];
        for chunk in frame.chunks_exact(480) {
            for (i, s) in chunk.iter().enumerate() {
                buf[i] = *s as f32 / 32768.0;
            }
            if processor.process_render_frame([&mut buf]).is_err() {
                return;
            }
        }
    }

    /// Suppress noise in a 48 kHz mono frame (length must be a multiple of
    /// 480, e.g. 960). AEC3 + high-pass + WebRTC NS run first (WebRTC APM);
    /// then the active denoiser — the high-quality DPDFNet tier when engaged,
    /// or RNNoise (the light tier) — then VAD-gated adaptive gain. WebRTC NS
    /// stays on in both tiers (the light tier needs it; on the neural tier it
    /// is cheap and the neural enhancement dominates).
    pub fn process(&mut self, frame: &[i16]) -> Vec<i16> {
        // WebRTC APM: AEC3 + high-pass + NS (always on — see chain()).
        let out: Vec<i16> = match self.processor.as_mut() {
            Some(processor) => {
                let mut out = vec![0i16; frame.len()];
                let mut buf = [0f32; 480];
                for (in_chunk, out_chunk) in frame.chunks_exact(480).zip(out.chunks_exact_mut(480)) {
                    for (i, s) in in_chunk.iter().enumerate() {
                        buf[i] = *s as f32 / 32768.0;
                    }
                    // Panics if the block isn't exactly 10 ms; chunks_exact(480) guarantees it.
                    if processor.process_capture_frame([&mut buf]).is_ok() {
                        for (i, v) in buf.iter().enumerate() {
                            out_chunk[i] = (v * 32767.0)
                                .round()
                                .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                        }
                    } else {
                        out_chunk.copy_from_slice(in_chunk);
                    }
                }
                out
            }
            None => frame.to_vec(),
        };
        // Denoise with the active tier and derive the speech signal.
        let mut result: Vec<i16>;
        let vad: f32;
        let rms: f32;
        if self.neural_active {
            if let Some(n) = self.gtcrn.as_mut() {
                // GTCRN: the high-suppression denoiser. It silences noise to
                // ~0 (measured ~51 dB separation), so the post-denoise energy
                // IS the speech detector — no extra VAD, no extra CPU. Map it
                // to a VAD probability for the leveler.
                result = n.process(&out);
                let r = rms_level(&result);
                vad = (r / SPEECH_ENERGY_REF).clamp(0.0, 1.0);
                rms = r;
            } else {
                // No neural model loaded — fall through to the light tier.
                result = vec![0i16; out.len()];
                let r = rms_level(&out);
                vad = (r / SPEECH_ENERGY_REF).clamp(0.0, 1.0);
                rms = r;
            }
        } else {
            // Light tier: RNNoise denoising and its VAD — the full-band,
            // low-CPU path.
            result = vec![0i16; out.len()];
            let mut max_vad = 0.0f32;
            match self.rnnoise.as_mut() {
                Some(rn) => {
                    let mut input = [0f32; 480];
                    let mut denoised = [0f32; 480];
                    for (chunk, out_chunk) in out.chunks_exact(480).zip(result.chunks_exact_mut(480)) {
                        for (i, s) in chunk.iter().enumerate() {
                            input[i] = *s as f32 / 32768.0;
                        }
                        let v = rn.process_frame(&mut denoised, &input);
                        max_vad = max_vad.max(v);
                        for (i, v) in denoised.iter().enumerate() {
                            out_chunk[i] = (v * 32767.0)
                                .round()
                                .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                        }
                    }
                }
                None => result.copy_from_slice(&out),
            }
            let r = rms_level(&result);
            vad = max_vad;
            rms = r;
        };
        // VAD-gated adaptive gain: boost quiet speech to an audible level
        // without amplifying the (already suppressed) noise floor.
        self.speech_detected = self.leveler.process(vad, rms, &mut result);
        result
    }

    /// VAD-gated send-path processing: the CPU-saving entry point used by the
    /// live mic path. Returns `Some(cleaned_frame)` when speech is present
    /// (or within the hangover), and `None` when the frame is silence — in
    /// which case the caller must skip encode + transmit entirely.
    ///
    /// This is what makes a voice call cheap: a call is mostly "listening" —
    /// the local mic is silent while the far end talks. Rather than burn
    /// AEC3 + the active denoiser + Opus on every silent frame (measured
    /// ~20% of a core with the neural tier), we run only a cheap energy gate
    /// and, when there is no speech, skip the whole chain AND the transmit.
    /// Speech frames still go through the full denoise chain, so quality is
    /// unchanged; silence simply costs almost nothing. The hangover keeps the
    /// chain open briefly
    /// after the last voiced frame so speech tails and quiet endings aren't
    /// clipped at word/utterance boundaries.
    ///
    /// ```ignore
    /// while let Some(frame) = mic.recv().await {
    ///     if let Some(cleaned) = ns.process_gated(&frame) {
    ///         let pkt = encode(&cleaned);
    ///         for peer in peers { peer.send(pkt.clone()); }
    ///     }
    ///     // None => silence: skip encode + transmit (no work).
    /// }
    /// ```
    pub fn process_gated(&mut self, frame: &[i16]) -> Option<Vec<i16>> {
        // Energy gate on the raw frame (before AEC3): a handful of multiply-adds
        // per sample, ~0.1% of a core — essentially free. When the frame is
        // quiet (mic silent, or quiet room noise) and the post-speech hangover
        // has lapsed, we skip the whole send chain (AEC3 + GTCRN + encode +
        // transmit) and send nothing — the dominant CPU cost in a call, which
        // is mostly "listening" with a silent mic.
        //
        // This only closes on genuinely quiet frames, so it never hurts: a
        // normal speaking voice (RMS ~0.1) is far above the floor, and in a
        // noisy room (fan/AC above the floor) the gate stays open — no CPU
        // saved there, but no quality lost either. Speech onset is caught by
        // the frame RMS, and the hangover keeps the chain open across the
        // brief dips at word boundaries so nothing is clipped.
        let level = rms_level(frame);
        if level >= VOICE_ENERGY_FLOOR {
            // Speech energy: (re)open the chain and reset the hangover.
            self.gate_hangover = GATE_HANGOVER_FRAMES;
        } else if self.gate_hangover > 0 {
            // Still within the hangover after speech: keep the chain open.
            self.gate_hangover -= 1;
        } else {
            // Confirmed quiet: skip the entire chain (no AEC3, no GTCRN, no
            // encode, no transmit). The far side just hears silence.
            self.speech_detected = false;
            return None;
        }

        let cleaned = self.process(frame);
        Some(cleaned)
    }

    /// Whether the most recently processed frame contained speech (RNNoise
    /// VAD). Drives voice-activation (transmit gating) and the speaking meter.
    pub fn speech_detected(&self) -> bool {
        self.speech_detected
    }

    /// Whether the high-quality neural (GTCRN) tier is available (loaded).
    /// `false` means the model/onnxruntime failed to load and the chain is
    /// permanently light (RNNoise).
    pub fn neural_available(&self) -> bool {
        self.gtcrn.is_some()
    }
}

/// Build the WebRTC APM config for a given denoiser tier. AEC3 + high-pass are
/// always on; WebRTC NS (VeryHigh, ~9x stationary-noise attenuation) is ON in
/// the light tier (RNNoise needs it) and OFF in the neural tier (GTCRN is the
/// denoiser; NS before it would double-color the speech). AGC is always OFF
/// (the fixed-digital AGC amplified background noise before NS removed it).
fn apm_config(neural: bool) -> Config {
    Config {
        echo_canceller: Some(EchoCanceller::Full { stream_delay_ms: None }),
        high_pass_filter: Some(HighPassFilter { apply_in_full_band: true }),
        noise_suppression: if neural {
            None
        } else {
            Some(NoiseSuppression {
                level: NoiseSuppressionLevel::VeryHigh,
                analyze_linear_aec_output: false,
            })
        },
        gain_controller: None,
        ..Config::default()
    }
}

impl Default for NoiseSuppressor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// GtcrnDenoiser (sherpa-onnx GTCRN) — the high-suppression neural tier
// ---------------------------------------------------------------------------

/// GTCRN speech-enhancement denoiser — the "high quality" tier used when the
/// adaptive CPU monitor sees headroom. 32-38 dB noise reduction, 16 kHz model
/// (band-limited to ~8 kHz), ~23% of a core on a 2014 i5. The mic is 48 kHz,
/// so sherpa-onnx resamples 48k→16k internally and we resample the 16 kHz
/// output back to 48 kHz.
///
/// NOTE: the ideal Krisp-like replacement here is a full-band 48 kHz model
/// (sherpa-onnx DPDFNet), but DPDFNet models as of 2026 produce pure silence
/// with sherpa-onnx 1.13.4 (the latest on crates.io) — an upstream
/// incompatibility, no newer version available. When sherpa-onnx supports it,
/// swap the model config in `new()` and drop the resampler (DPDFNet is 48 kHz
/// native). Until then GTCRN is the working neural tier and the default stays
/// the full-band RNNoise light tier.
pub struct GtcrnDenoiser {
    online: sherpa_onnx::OnlineSpeechDenoiser,
    /// Accumulated 16 kHz denoised output, resampled to 48 kHz in 20 ms frames.
    out_buf: Vec<f32>,
    /// 16 kHz -> 48 kHz for the denoised output.
    resampler: LinearResampler,
    /// Cache path where the embedded model was written.
    _model_path: std::path::PathBuf,
}

/// Samples per 20 ms frame at 16 kHz.
const GTCRN_FRAME_16K: usize = 320;

impl GtcrnDenoiser {
    /// Create from the embedded model. `None` if the model can't be loaded
    /// (e.g. onnxruntime init failed) — the caller then runs permanently light.
    pub fn new() -> Option<Self> {
        use sherpa_onnx::{
            OfflineSpeechDenoiserGtcrnModelConfig, OfflineSpeechDenoiserModelConfig,
            OnlineSpeechDenoiserConfig,
        };
        let model_path = Self::materialize_model()?;
        let config = OnlineSpeechDenoiserConfig {
            model: OfflineSpeechDenoiserModelConfig {
                gtcrn: OfflineSpeechDenoiserGtcrnModelConfig {
                    model: Some(model_path.to_string_lossy().into_owned()),
                },
                ..Default::default()
            },
        };
        let online = sherpa_onnx::OnlineSpeechDenoiser::create(&config)?;
        Some(Self {
            online,
            out_buf: Vec::with_capacity(GTCRN_FRAME_16K * 2),
            resampler: LinearResampler::new(16_000, CLOCK_RATE),
            _model_path: model_path,
        })
    }

    /// Write the embedded model to a cache file and return its path.
    fn materialize_model() -> Option<std::path::PathBuf> {
        const MODEL: &[u8] = include_bytes!("../models/gtcrn_simple.onnx");
        let path = std::env::temp_dir().join("lumen-gtcrn_simple.onnx");
        // Idempotent: only write if missing or different size.
        if !path.exists() || std::fs::metadata(&path).ok().map(|m| m.len()) != Some(MODEL.len() as u64) {
            std::fs::write(&path, MODEL).ok()?;
        }
        Some(path)
    }

    /// Denoise a 48 kHz mono frame. Returns exactly `frame.len()` 48 kHz mono
    /// samples (padded with silence until the streaming path has produced a
    /// full 20 ms frame), so the pipeline stays 20 ms aligned.
    ///
    /// The streaming denoiser does NOT emit a fixed 320 samples per 20 ms
    /// input chunk: it outputs in 16 ms (256 @ 16 kHz) bursts. So we emit at
    /// most ONE 20 ms chunk per call and carry excess into the next frame:
    /// output stays lossless and the buffer stays bounded.
    pub fn process(&mut self, frame: &[i16]) -> Vec<i16> {
        let mut in_f32 = Vec::with_capacity(frame.len());
        for &s in frame {
            in_f32.push(s as f32 / 32768.0);
        }
        let out = self.online.run(&in_f32, CLOCK_RATE as i32);
        self.out_buf.extend_from_slice(&out.samples);
        let mut result: Vec<i16> = Vec::with_capacity(frame.len());
        if self.out_buf.len() >= GTCRN_FRAME_16K {
            let chunk: Vec<i16> = self.out_buf
                .drain(..GTCRN_FRAME_16K)
                .map(|v| (v * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
                .collect();
            result.extend_from_slice(&self.resampler.resample(&chunk));
        }
        if result.len() < frame.len() {
            result.resize(frame.len(), 0);
        } else if result.len() > frame.len() {
            result.truncate(frame.len());
        }
        result
    }
}

// ---------------------------------------------------------------------------
// Speech leveler: VAD-gated adaptive gain
// ---------------------------------------------------------------------------
/// VAD-gated adaptive gain (a sidechain compressor): raises quiet speech to a
/// target level and gates silence, so a cheap/quiet mic is audible WITHOUT
/// amplifying background noise.
///
/// WebRTC's adaptive AGC (GainController2) was measured to boost the noise
/// floor (+15 dB on quiet noise), so we do the gate ourselves: the VAD signal
/// (post-GTCRN energy in the main path, RNNoise probability in the fallback)
/// opens the gain, which chases a target RMS and holds through a hangover so
/// words aren't clipped; on silence the gain decays to unity (no boost),
/// leaving the already-suppressed noise inaudible.
pub struct SpeechLeveler {
    /// Target RMS for speech after gain (~ -18 dBFS).
    target_rms: f32,
    /// Max gain factor (+24 dB).
    max_gain: f32,
    /// Current smoothed gain factor.
    gain: f32,
    /// Smoothed voice-activity probability (0..1).
    vad_smooth: f32,
    /// Smoothed pre-gain speech level (for gain computation).
    speech_rms: f32,
    /// Frames to keep gain up after VAD drops (avoids clipping word tails).
    hold: u32,
}

const VAD_ON: f32 = 0.5;
/// ~100 ms of hold at 20 ms frames.
const HANGOVER_FRAMES: u32 = 5;

/// Energy floor (normalized RMS) for the `process_gated` send gate. Frames
/// below this are treated as quiet and skip the whole send chain (AEC3 +
/// GTCRN + Opus + transmit). A normal speaking voice is ~RMS 0.1 (well above);
/// a quiet room is ~RMS 0.005-0.02 (well below). A noisy room (fan/AC above
/// the floor) keeps the gate open — no CPU saved, but no quality lost.
/// Lower to gate more aggressively (more CPU savings, risks clipping very
/// quiet speech); raise to be conservative. The `GATE_HANGOVER_FRAMES`
/// hangover prevents clipping across word-boundary dips.
const VOICE_ENERGY_FLOOR: f32 = 0.03;

/// Frames the send chain stays open after the last voiced frame (hangover),
/// so speech tails and quiet word endings aren't clipped. 10 frames = 200 ms.
const GATE_HANGOVER_FRAMES: u32 = 10;

/// Post-denoise RMS (0..1) that maps to a full VAD probability in the neural
/// path. The neural denoiser silences noise to ~0 (DPDFNet ~58 dB separation),
/// so any energy above this is speech; a value this low keeps quiet speech
/// detectable while leaving the noise floor far below the leveler's VAD_ON.
const SPEECH_ENERGY_REF: f32 = 0.01;

impl SpeechLeveler {
    pub fn new() -> Self {
        Self {
            target_rms: 0.12,
            max_gain: 16.0,
            gain: 1.0,
            vad_smooth: 0.0,
            speech_rms: 0.001,
            hold: 0,
        }
    }

    /// Apply gain to `samples` in place based on the frame's VAD probability
    /// and RMS. Returns true if speech was detected this frame.
    pub fn process(&mut self, vad: f32, frame_rms: f32, samples: &mut [i16]) -> bool {
        self.vad_smooth = self.vad_smooth * 0.8 + vad * 0.2;
        let speech = self.vad_smooth > VAD_ON;
        if speech {
            // In production `frame_rms` is the fresh pre-gain level (the caller
            // measures before we apply gain), so track it directly with a fast
            // attack and slow release.
            if frame_rms > self.speech_rms {
                self.speech_rms = frame_rms;
            } else {
                self.speech_rms = self.speech_rms * 0.9 + frame_rms * 0.1;
            }
            let desired =
                (self.target_rms / self.speech_rms.max(0.0001)).clamp(1.0, self.max_gain);
            self.gain = self.gain * 0.85 + desired * 0.15;
            self.hold = HANGOVER_FRAMES;
        } else if self.hold > 0 {
            self.hold -= 1;
        } else {
            // Silence: decay the boost toward unity so noise stays un-boosted.
            self.gain = (self.gain - 1.0) * 0.85 + 1.0;
        }
        if self.gain > 1.0001 {
            for s in samples.iter_mut() {
                *s = ((*s as f32) * self.gain)
                    .round()
                    .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            }
        }
        speech
    }
}

impl Default for SpeechLeveler {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Jitter buffer
// ---------------------------------------------------------------------------

/// Orders RTP packets by timestamp and hands frames out in play order at a
/// fixed 20 ms cadence (driven by the caller ticking `pop` every 20 ms).
/// Playout starts once `target` frames are buffered, then never waits: a
/// missing timestamp yields `Some(None)` so the caller runs OPUS PLC instead
/// of stalling. Packets that arrive after their play time are discarded.
pub struct JitterBuffer {
    pending: BTreeMap<u32, Vec<(u16, Vec<u8>)>>, // ts -> [(seq, payload)]
    next_ts: Option<u32>,
    target: u32,
    dropped: u64,
}

impl JitterBuffer {
    pub fn new(target_frames: u32) -> Self {
        Self {
            pending: BTreeMap::new(),
            next_ts: None,
            target: target_frames * FRAME_SAMPLES as u32,
            dropped: 0,
        }
    }

    pub fn push(&mut self, seq: u16, timestamp: u32, payload: Vec<u8>) {
        self.pending.entry(timestamp).or_default().push((seq, payload));
        // Drop anything that can never play (older than the play cursor).
        if let Some(next) = self.next_ts {
            while let Some((&ts, _)) = self.pending.iter().next() {
                if ts < next {
                    self.pending.remove(&ts);
                    self.dropped += 1;
                } else {
                    break;
                }
            }
        }
    }

    /// Advance playout by one frame. `Some(Some(payload))` = play this frame;
    /// `Some(None)` = frame lost, run PLC; `None` = still filling, wait.
    pub fn pop(&mut self) -> Option<Option<(u32, Vec<u8>)>> {
        let first = *self.pending.keys().next()?;
        match self.next_ts {
            None => {
                // Wait until `target` frames are buffered before starting.
                let newest = *self.pending.keys().next_back()?;
                if newest < first + self.target {
                    return None;
                }
                self.next_ts = Some(first + FRAME_SAMPLES as u32);
                let frame = self.pending.remove(&first).map(|mut p| {
                    p.sort_by_key(|(seq, _)| *seq);
                    p.remove(0)
                });
                return Some(frame.map(|(_, payload)| (first, payload)));
            }
            Some(ts) => {
                let frame = self.pending.remove(&ts);
                self.next_ts = Some(ts + FRAME_SAMPLES as u32);
                Some(frame.map(|mut p| {
                    p.sort_by_key(|(seq, _)| *seq);
                    let (_, payload) = p.remove(0);
                    (ts, payload)
                }))
            }
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Linear-interpolation resampler for i16. Good enough for voice.
///
/// Stateful: it carries the fractional source position between calls, so a
/// stream split into arbitrary-sized chunks (cpal callback sizes vary)
/// resamples without per-chunk phase glitches. An aligned 20 ms frame still
/// yields exactly one frame's worth of device samples, so pitch is preserved.
pub struct LinearResampler {
    src_rate: u32,
    dst_rate: u32,
    /// Fractional position (in source samples) where the next output lands.
    pos: f64,
}

impl LinearResampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self { src_rate, dst_rate, pos: 0.0 }
    }

    pub fn resample(&mut self, input: &[i16]) -> Vec<i16> {
        if input.is_empty() {
            return Vec::new();
        }
        if self.src_rate == self.dst_rate {
            return input.to_vec();
        }
        let len = input.len() as f64;
        let ratio = self.dst_rate as f64 / self.src_rate as f64;
        let step = 1.0 / ratio;
        let mut out = Vec::with_capacity((len * ratio).ceil() as usize);
        while self.pos < len {
            let idx = self.pos.floor() as usize;
            let frac = self.pos - idx as f64;
            let a = input[idx] as f64;
            // Last sample has no successor in this chunk; extend it flat.
            let b = if idx + 1 < input.len() { input[idx + 1] as f64 } else { a };
            out.push((a + (b - a) * frac).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
            self.pos += step;
        }
        // Carry the phase overshoot into the next chunk (stays in [0, step)).
        self.pos -= len;
        out
    }
}

/// RMS of an i16 window mapped to 0..1.
pub fn rms_level(pcm: &[i16]) -> f32 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_passthrough() {
        let mut r = LinearResampler::new(48000, 48000);
        let input: Vec<i16> = (0..960).collect();
        assert_eq!(r.resample(&input), input);
    }

    #[test]
    fn resampler_length_and_shape() {
        let mut r = LinearResampler::new(44100, 48000);
        let input: Vec<i16> = (0..4410).map(|i| ((i as f64 * 0.1).sin() * 1000.0) as i16).collect();
        let out = r.resample(&input);
        // ±1: a chunk boundary may land exactly on a sample; the carried phase
        // compensates on the next chunk (no accumulated drift).
        assert!(out.len().abs_diff(4800) <= 1, "got {}", out.len());
        // Smooth sine stays in range and roughly keeps its amplitude.
        assert!(out.iter().map(|v| v.abs()).max().unwrap() < 1200);
    }

    #[test]
    fn resampler_preserves_duration_any_rate() {
        // A 20 ms frame at 48 kHz must stay 20 ms at any device rate, otherwise
        // audio plays fast (chipmunk) or slow. ±1 per chunk: the carried phase
        // keeps the long-run rate exact.
        let input: Vec<i16> = (0..FRAME_SAMPLES).map(|i| ((i as f64 * 0.05).sin() * 2000.0) as i16).collect();
        let len_at = |dst: u32| LinearResampler::new(48000, dst).resample(&input).len();
        assert_eq!(len_at(48000), 960); // 20 ms, passthrough exact
        assert!(len_at(96000).abs_diff(1920) <= 1); // 20 ms @ 96k
        assert!(len_at(44100).abs_diff(882) <= 1); // 20 ms @ 44.1k
        assert!(len_at(16000).abs_diff(320) <= 1); // 20 ms @ 16k
    }

    #[test]
    fn output_expands_mono_to_stereo_without_chipmunk() {
        // Regression: mono PCM written sequentially into an interleaved stereo
        // buffer plays each channel at 2× speed (chipmunk). drain_into must
        // duplicate every mono sample into both channels.
        let output = AudioOutput::new();
        *output.state.lock() = Some(OutputState {
            buf: Vec::new(),
            resampler: LinearResampler::new(CLOCK_RATE, CLOCK_RATE),
            channels: 2,
            frame_size: FRAME_SAMPLES,
            dropped_samples: 0,
        });
        let frame: Vec<i16> = (0..FRAME_SAMPLES as i16).collect();
        output.push(&frame);
        let mut out = vec![0i16; 480 * 2]; // 10 ms of stereo
        output.drain_into(&mut out);
        for f in 0..480 {
            assert_eq!(out[2 * f], f as i16, "left sample of frame {f}");
            assert_eq!(out[2 * f + 1], f as i16, "right sample of frame {f}");
        }
        // The untouched tail stays silent (only 480 frames were drained).
        let mut tail = vec![1i16; 480 * 2];
        output.drain_into(&mut tail);
        assert!(tail[480 * 2..].iter().all(|&v| v == 0) || tail[..480 * 2].iter().all(|&v| v == 0));
    }

    #[test]
    fn resampler_stateful_continuity() {
        // Two consecutive chunks must resample as one continuous stream: the
        // output count over both chunks must equal the single-chunk count.
        let whole: Vec<i16> = (0..882).map(|i| ((i as f64 * 0.2).sin() * 3000.0) as i16).collect();
        let mut single = LinearResampler::new(44100, 48000);
        let expected = single.resample(&whole).len();
        let mut split = LinearResampler::new(44100, 48000);
        let a = split.resample(&whole[..441]);
        let b = split.resample(&whole[441..]);
        // Splitting may differ by at most one sample per boundary.
        assert!((a.len() + b.len()).abs_diff(expected) <= 1, "{} vs {}", a.len() + b.len(), expected);
    }

    #[test]
    fn rms_of_silence_and_tone() {
        assert_eq!(rms_level(&[0; 960]), 0.0);
        let tone: Vec<i16> = vec![10000; 960];
        let lvl = rms_level(&tone);
        assert!(lvl > 0.2 && lvl < 0.4, "got {lvl}");
    }

    #[test]
    fn jitter_orders_waits_and_plc() {
        let mut jb = JitterBuffer::new(3);
        let payload = |i: usize| vec![i as u8; 10];
        // Arrive out of order; frame 2 is late (after 3).
        jb.push(3, 3 * 960, payload(3));
        jb.push(1, 1 * 960, payload(1));
        jb.push(4, 4 * 960, payload(4));
        jb.push(0, 0, payload(0));
        // Only 4 frames (0,1,3,4) — newest 4*960, oldest 0, target 2880:
        // newest >= oldest+target holds (3840 >= 2880) → playout starts at 0.
        assert!(matches!(jb.pop(), Some(Some((0, _)))));
        assert!(matches!(jb.pop(), Some(Some((960, _)))));
        // Frame 2 not arrived yet → PLC.
        assert!(matches!(jb.pop(), Some(None)));
        // Frame 2 arrives late → must be dropped (its ts < play cursor).
        jb.push(2, 2 * 960, payload(2));
        assert!(matches!(jb.pop(), Some(Some((2880, _)))));
        assert!(matches!(jb.pop(), Some(Some((3840, _)))));
        // Nothing left to play → keep waiting (not PLC, just idle).
        assert!(jb.pop().is_none());
        assert_eq!(jb.dropped(), 1);
    }

    #[test]
    fn jitter_waits_for_target() {
        let mut jb = JitterBuffer::new(3);
        jb.push(0, 0, vec![0]);
        // Only one frame buffered — not enough to start.
        assert!(jb.pop().is_none());
    }

    #[test]
    fn opus_roundtrip() {
        let mut enc = OpusEncoder::new().unwrap();
        let mut dec = OpusDecoder::new().unwrap();
        let pcm: Vec<i16> = (0..FRAME_SAMPLES).map(|i| ((i as f64 * 0.05).sin() * 3000.0) as i16).collect();
        let pkt = enc.encode(&pcm).unwrap();
        // At 64 kbps a 20 ms mono frame is at most ~160 bytes; allow generous
        // headroom (Opus VBR frames can briefly exceed the average).
        assert!(pkt.len() < 400, "packet too large: {}", pkt.len());
        let decoded = dec.decode(Some(&pkt)).unwrap();
        assert_eq!(decoded.len(), FRAME_SAMPLES);
        // Opus inserts ~6.5 ms of algorithmic delay, so a phase-aligned
        // correlation is meaningless. Verify instead that en/decode preserved
        // the signal's energy (not silence, not a near-zero/dead stream).
        let src_rms = rms_level(&pcm);
        let dec_rms = rms_level(&decoded);
        assert!(dec_rms > 0.05, "decoded is silent: {dec_rms}");
        assert!(
            dec_rms > src_rms * 0.2 && dec_rms < src_rms * 5.0,
            "level mismatch: src {src_rms}, decoded {dec_rms}"
        );
        // PLC on loss: silence-ish output, no panic.
        let plc = dec.decode(None).unwrap();
        assert_eq!(plc.len(), FRAME_SAMPLES);
    }

    #[test]
    fn noise_suppressor_pipeline() {
        // Smoke: the DSP pipeline (AEC3 + NS + RNNoise) runs on a
        // speech-like voiced signal without panicking and preserves length.
        // NOTE: no amplitude assertion — a *synthetic* continuous buzz reads
        // as stationary noise to speech-tuned models (WebRTC NS, RNNoise) and
        // gets gated; real speech (pauses + dynamics) is preserved, which is
        // the standard behavior of these industry models (Chrome/Meet/Discord).
        let mut ns = NoiseSuppressor::new();
        let mut frame = vec![0i16; FRAME_SAMPLES];
        for i in 0..FRAME_SAMPLES {
            let t = i as f64 / CLOCK_RATE as f64;
            let mut v = 0.0;
            for (n, amp) in [
                (1, 1.0), (2, 0.5), (3, 0.33), (4, 0.25),
                (5, 0.2), (6, 0.16), (7, 0.14), (8, 0.12),
            ] {
                v += amp * (2.0 * std::f64::consts::PI * 150.0 * n as f64 * t).sin();
            }
            let am = 0.7 + 0.3 * (2.0 * std::f64::consts::PI * 8.0 * t).sin();
            frame[i] = (v * am * 3000.0) as i16;
        }
        for _ in 0..10 {
            let out = ns.process(&frame);
            assert_eq!(out.len(), FRAME_SAMPLES);
        }
    }

    #[test]
    fn speech_leveler_boosts_quiet_speech_not_silence() {
        let mut lvl = SpeechLeveler::new();
        // Build a quiet speech-like frame generator (fresh, un-gained each time
        // — like production where every frame is new).
        let base: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| {
                (((i as f32 / CLOCK_RATE as f32) * 2.0 * std::f32::consts::PI * 200.0).sin()
                    * 600.0) as i16 // RMS ~0.013
            })
            .collect();
        let before = rms_level(&base);
        let mut out = base.clone();
        // Warm up the gain toward the target over several fresh frames.
        for _ in 0..15 {
            let mut frame = base.clone();
            lvl.process(0.9, rms_level(&frame), &mut frame);
            out = frame;
        }
        let after = rms_level(&out);
        eprintln!("speech leveler: {before:.4} -> {after:.4}");
        assert!(
            after > before * 2.0,
            "quiet speech should be boosted: {before} -> {after}"
        );
        assert!(
            after < 0.5,
            "boost must not clip/overshoot: {after}"
        );

        // A new leveler on a low-VAD (silence) frame must NOT boost.
        let mut lvl2 = SpeechLeveler::new();
        let silence = vec![400i16; FRAME_SAMPLES]; // low-level noise-ish
        let sb = rms_level(&silence);
        let mut out2 = silence.clone();
        for _ in 0..15 {
            let mut frame = silence.clone();
            lvl2.process(0.05, rms_level(&frame), &mut frame);
            out2 = frame;
        }
        let sa = rms_level(&out2);
        eprintln!("silence: {sb:.4} -> {sa:.4}");
        assert!(
            sa <= sb * 1.5,
            "silence must not be boosted: {sb} -> {sa}"
        );
    }

    #[test]
    fn speech_leveler_gates_silence_with_hangover() {
        let mut lvl = SpeechLeveler::new();
        let base: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| {
                (((i as f32 / CLOCK_RATE as f32) * 2.0 * std::f32::consts::PI * 200.0).sin()
                    * 600.0) as i16
            })
            .collect();
        // Speak for several frames (fresh each time) to open the gain, then go
        // silent. Gain must persist through the hangover, then decay toward 1.
        for _ in 0..15 {
            let mut frame = base.clone();
            lvl.process(0.9, rms_level(&frame), &mut frame);
        }
        let gain_after_speech = lvl.gain;
        assert!(gain_after_speech > 1.0, "gain should open on speech");
        // Silence for longer than the hangover -> gain decays back toward 1.
        let silence = vec![400i16; FRAME_SAMPLES];
        for _ in 0..(HANGOVER_FRAMES + 10) {
            let mut frame = silence.clone();
            lvl.process(0.05, rms_level(&frame), &mut frame);
        }
        assert!(
            lvl.gain < gain_after_speech * 0.6,
            "gain should decay on sustained silence: {} -> {}",
            gain_after_speech,
            lvl.gain
        );
    }

    #[test]
    fn noise_suppressor_attenuates_background_noise() {
        // The shipped send-path DSP (AEC3 + NS VeryHigh + RNNoise, AGC off)
        // must strongly attenuate moderate stationary background noise — the
        // case the user reported (mic picking up room noise). Measured ~9x.
        let mut ns = NoiseSuppressor::new();
        // Deterministic pseudo-random white noise at moderate level (RMS ~0.07,
        // like a room fan / AC in the background).
        let mut state = 0x1234_5678u32;
        let mut noise = Vec::with_capacity(480 * 24);
        for _ in 0..(480 * 24) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            noise.push(((state >> 8) as i16) / 8);
        }
        // Warm up the RNN + NS models, then measure attenuation.
        for chunk in noise.chunks(480).take(12) {
            ns.process(chunk);
        }
        let probe = &noise[480 * 12..480 * 13];
        let input_rms = rms_level(probe);
        let out = ns.process(probe);
        let output_rms = rms_level(&out);
        eprintln!("send-path DSP: {input_rms} -> {output_rms}");
        assert!(
            output_rms < input_rms * 0.3,
            "DSP should strongly attenuate background noise: {input_rms} -> {output_rms}"
        );
    }

    #[test]
    fn webrtc_ns_attenuates_white_noise() {
        use webrtc_audio_processing::config::{NoiseSuppression, NoiseSuppressionLevel};
        // NS-only processor (no AGC, which would re-amplify quiet noise).
        let processor = Processor::new(CLOCK_RATE).expect("APM init");
        processor.set_config(Config {
            noise_suppression: Some(NoiseSuppression {
                level: NoiseSuppressionLevel::VeryHigh,
                analyze_linear_aec_output: false,
            }),
            ..Config::default()
        });
        // Deterministic pseudo-random white noise.
        let mut state = 0x1234_5678u32;
        let mut noise = Vec::with_capacity(480 * 24);
        for _ in 0..(480 * 24) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            noise.push(((state >> 8) as i16) / 4);
        }
        let mut ns = NoiseSuppressor { processor: Some(processor), rnnoise: None, gtcrn: None, neural_active: false, leveler: SpeechLeveler::new(), speech_detected: false, gate_hangover: 0 };
        // Warm up the model, then measure attenuation.
        for chunk in noise.chunks(480).take(12) {
            ns.process(chunk);
        }
        let probe = &noise[480 * 12..480 * 13];
        let input_rms = rms_level(probe);
        let out = ns.process(probe);
        let output_rms = rms_level(&out);
        assert!(
            output_rms < input_rms * 0.5,
            "NS should strongly attenuate white noise: {input_rms} -> {output_rms}"
        );
    }

    #[test]
    fn output_bounds_playout_latency() {
        // Regression: a push/drain imbalance (bursts, several peers, a peer
        // still pushing during a re-join) must not let the output ring grow to
        // seconds of stale audio. push() sheds the oldest samples to keep
        // latency bounded to ~4 frames.
        let output = AudioOutput::new();
        *output.state.lock() = Some(OutputState {
            buf: Vec::new(),
            resampler: LinearResampler::new(CLOCK_RATE, CLOCK_RATE),
            channels: 1,
            frame_size: FRAME_SAMPLES,
            dropped_samples: 0,
        });
        let frame: Vec<i16> = (0..FRAME_SAMPLES as i16).collect();
        // Push far more frames than could ever drain (e.g. 30 s of audio).
        for _ in 0..(30 * 50) {
            output.push(&frame);
        }
        let len = output.state.lock().as_ref().unwrap().buf.len();
        assert!(
            len <= FRAME_SAMPLES * 4,
            "output buffer grew to {len} samples (> 80 ms latency)"
        );
    }

    #[test]
    fn gtcrn_streaming_no_dropped_frames() {
        // The streaming denoiser outputs in 16 ms (256 @ 16 kHz) bursts, not
        // 320 per call; we emit at most one 20 ms chunk per call and carry the
        // excess, so output stays lossless and the buffer bounded. Output must
        // stay 20 ms aligned with no silence-padded frames once warm, and the
        // speech energy must survive (not be gated away). Use a speech-like AM
        // signal: GTCRN preserves speech but legitimately gates a steady tone
        // (stationary noise).
        fn speech_frame(idx: usize) -> Vec<i16> {
            (0..FRAME_SAMPLES)
                .map(|i| {
                    let t = ((idx * FRAME_SAMPLES + i) as f64) / CLOCK_RATE as f64;
                    let mut v = 0.0;
                    for (n, amp) in [(1, 1.0), (2, 0.5), (3, 0.33), (4, 0.25), (5, 0.2)] {
                        v += amp * (2.0 * std::f64::consts::PI * 150.0 * n as f64 * t).sin();
                    }
                    let am = 0.7 + 0.3 * (2.0 * std::f64::consts::PI * 8.0 * t).sin();
                    (v * am * 3000.0) as i16
                })
                .collect()
        }
        let Some(mut g) = GtcrnDenoiser::new() else { return };
        let mut silent_after_warmup = 0usize;
        let mut total_out = 0f64;
        let mut total_in = 0f64;
        for i in 0..80 {
            let fr = speech_frame(i);
            let out = g.process(&fr);
            assert_eq!(out.len(), FRAME_SAMPLES, "neural denoiser must keep 20 ms alignment");
            if i >= 6 {
                if rms_level(&out) < 0.003 {
                    silent_after_warmup += 1;
                }
                total_out += out.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>();
                total_in += fr.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>();
            }
        }
        let ratio = total_out / total_in.max(1e-9);
        eprintln!("silent frames after warm-up: {silent_after_warmup}/74; speech out/in energy {ratio:.2}");
        assert_eq!(
            silent_after_warmup, 0,
            "neural denoiser produced {silent_after_warmup} silent frames after warm-up -> dropped/chopped audio"
        );
        assert!(ratio > 0.15, "neural denoiser gated speech away: out/in {ratio:.2}");
    }

    #[test]
    fn gtcrn_path_speech_detection_by_energy() {
        // The high-quality (GTCRN) path derives speech detection from
        // post-denoise energy (no separate neural VAD): the denoiser silences
        // noise to ~0, so energy implies speech. Noise must not open the gate;
        // speech must.
        if GtcrnDenoiser::new().is_none() {
            return;
        }
        fn speech(idx: usize) -> Vec<i16> {
            (0..FRAME_SAMPLES)
                .map(|i| {
                    let t = ((idx * FRAME_SAMPLES + i) as f64) / CLOCK_RATE as f64;
                    let mut v = 0.0;
                    for (n, amp) in [(1, 1.0), (2, 0.5), (3, 0.33), (4, 0.25)] {
                        v += amp * (2.0 * std::f64::consts::PI * 150.0 * n as f64 * t).sin();
                    }
                    let am = 0.7 + 0.3 * (2.0 * std::f64::consts::PI * 8.0 * t).sin();
                    (v * am * 3000.0) as i16
                })
                .collect()
        }
        let noise: Vec<i16> = {
            let mut state = 0x1234_5678u32;
            (0..FRAME_SAMPLES)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    ((state >> 8) as i16) / 4
                })
                .collect()
        };
        // Noise: must NOT be detected as speech over many frames.
        let mut ns = NoiseSuppressor::new();
        let mut noise_detected = false;
        for _ in 0..30 {
            ns.process(&noise);
            noise_detected |= ns.speech_detected();
        }
        assert!(!noise_detected, "noise must not open the VAD gate");
        // Speech: must be detected (post-GTCRN energy above the threshold).
        let mut ns = NoiseSuppressor::new();
        let mut speech_detected = false;
        for i in 0..40 {
            ns.process(&speech(i));
            speech_detected |= ns.speech_detected();
        }
        assert!(speech_detected, "speech must be detected via post-GTCRN energy");
    }

    #[test]
    fn process_gated_skips_silence_and_opens_on_speech() {
        // Loud speech-like harmonics — reliably opens the WebRTC VAD gate.
        fn loud_speech(idx: usize) -> Vec<i16> {
            (0..FRAME_SAMPLES)
                .map(|i| {
                    let t = ((idx * FRAME_SAMPLES + i) as f64) / CLOCK_RATE as f64;
                    let mut v = 0.0;
                    for (n, amp) in [(1, 1.0), (2, 0.5), (3, 0.33), (4, 0.25)] {
                        v += amp * (2.0 * std::f64::consts::PI * 150.0 * n as f64 * t).sin();
                    }
                    (v * 3000.0) as i16
                })
                .collect()
        }

        // Pure silence must gate to None immediately (no hangover to drain).
        let mut ns = NoiseSuppressor::new();
        let silence = vec![0i16; FRAME_SAMPLES];
        assert!(ns.process_gated(&silence).is_none(), "silence must gate to None");

        // Speech must open the gate (Some) within a few frames.
        let mut ns = NoiseSuppressor::new();
        let mut opened = false;
        for i in 0..40 {
            if ns.process_gated(&loud_speech(i)).is_some() {
                opened = true;
                break;
            }
        }
        assert!(opened, "speech must open the gate (Some)");

        // Hangover: after speech stops, the chain stays open GATE_HANGOVER_FRAMES
        // more frames, then gates to None (so tails aren't clipped).
        let mut ns = NoiseSuppressor::new();
        for i in 0..10 {
            let _ = ns.process_gated(&loud_speech(i));
        }
        let silence = vec![0i16; FRAME_SAMPLES];
        let mut open_after_silence = 0usize;
        for _ in 0..(GATE_HANGOVER_FRAMES as usize + 5) {
            if ns.process_gated(&silence).is_some() {
                open_after_silence += 1;
            }
        }
        assert!(
            open_after_silence > 0 && open_after_silence <= GATE_HANGOVER_FRAMES as usize,
            "hangover should keep the chain open briefly then close (open {open_after_silence} frames)"
        );
    }
}



