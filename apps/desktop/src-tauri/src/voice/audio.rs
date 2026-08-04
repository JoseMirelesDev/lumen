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
}

struct OutputState {
    /// Device-rate mono samples awaiting playback.
    buf: Vec<i16>,
    /// 48 kHz -> device rate, applied to each decoded frame on push.
    resampler: LinearResampler,
}

impl AudioOutput {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
            enabled: Arc::new(AtomicBool::new(true)),
        }
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
    /// device rate.
    pub fn push(&self, frame: &[i16]) {
        let mut guard = self.state.lock();
        let Some(st) = guard.as_mut() else { return };
        st.buf.extend_from_slice(&st.resampler.resample(frame));
        let excess = st.buf.len().saturating_sub(48_000 * 4); // 4 s safety ceiling
        if excess > 0 {
            st.buf.drain(..excess);
        }
    }

    /// Copy the available device-rate samples into `out` (FIFO), zero-filling
    /// the tail. Returns how many samples were copied.
    fn drain_into(&self, out: &mut [i16]) -> usize {
        let mut guard = self.state.lock();
        let Some(st) = guard.as_mut() else {
            out.fill(0);
            return 0;
        };
        let n = out.len().min(st.buf.len());
        out[..n].copy_from_slice(&st.buf[..n]);
        st.buf.drain(..n);
        for v in &mut out[n..] {
            *v = 0;
        }
        n
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
        encoder.set_bitrate(opus::Bitrate::Bits(32_000))?;
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

    /// Decode one packet into 20 ms of mono PCM. `None` packet = PLC.
    pub fn decode(&mut self, packet: Option<&[u8]>) -> anyhow::Result<Vec<i16>> {
        let mut out = vec![0i16; FRAME_SAMPLES];
        self.decoder.decode(packet.unwrap_or(&[]), &mut out, false)?;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Jitter buffer
// ---------------------------------------------------------------------------

/// Orders RTP packets by timestamp and hands frames out in play order at a
/// fixed 20 ms cadence (driven by the caller ticking `pop` every 20 ms).
///
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
/// Resamples one contiguous input chunk as an independent segment. Audio is
/// pushed to playback in aligned 20 ms frames, so per-chunk resampling covers
/// exactly one frame worth of device samples and preserves pitch at any device
/// rate (no time-compression / chipmunk).
pub struct LinearResampler {
    src_rate: u32,
    dst_rate: u32,
}

impl LinearResampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self { src_rate, dst_rate }
    }

    pub fn resample(&self, input: &[i16]) -> Vec<i16> {
        if input.is_empty() {
            return Vec::new();
        }
        if self.src_rate == self.dst_rate {
            return input.to_vec();
        }
        let ratio = self.dst_rate as f64 / self.src_rate as f64;
        let out_len = ((input.len() as f64) * ratio).ceil() as usize;
        let mut out = Vec::with_capacity(out_len);
        let mut pos = 0.0f64;
        for _ in 0..out_len {
            let idx = (pos.floor() as usize).min(input.len() - 1);
            let frac = pos - idx as f64;
            let a = input[idx] as f64;
            let b = if idx + 1 < input.len() { input[idx + 1] as f64 } else { a };
            out.push((a + (b - a) * frac).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
            pos += 1.0 / ratio;
        }
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
        let r = LinearResampler::new(48000, 48000);
        let input: Vec<i16> = (0..960).collect();
        assert_eq!(r.resample(&input), input);
    }

    #[test]
    fn resampler_length_and_shape() {
        let r = LinearResampler::new(44100, 48000);
        let input: Vec<i16> = (0..4410).map(|i| ((i as f64 * 0.1).sin() * 1000.0) as i16).collect();
        let out = r.resample(&input);
        assert_eq!(out.len(), 4800);
        // Smooth sine stays in range and roughly keeps its amplitude.
        assert!(out.iter().map(|v| v.abs()).max().unwrap() < 1200);
    }

    #[test]
    fn resampler_preserves_duration_any_rate() {
        // A 20 ms frame at 48 kHz must stay 20 ms at any device rate, otherwise
        // audio plays fast (chipmunk) or slow.
        let input: Vec<i16> = (0..FRAME_SAMPLES).map(|i| ((i as f64 * 0.05).sin() * 2000.0) as i16).collect();
        assert_eq!(LinearResampler::new(48000, 48000).resample(&input).len(), 960); // 20 ms
        assert_eq!(LinearResampler::new(48000, 96000).resample(&input).len(), 1920); // 20 ms @ 96k
        assert_eq!(LinearResampler::new(48000, 44100).resample(&input).len(), 882); // 20 ms @ 44.1k
        assert_eq!(LinearResampler::new(48000, 16000).resample(&input).len(), 320); // 20 ms @ 16k
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
        assert!(pkt.len() < 150, "packet too large: {}", pkt.len());
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
}
