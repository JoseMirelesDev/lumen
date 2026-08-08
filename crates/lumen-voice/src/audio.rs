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

/// Handle to the running mic capture (cpal, or the Windows WASAPI raw path).
/// Dropping it stops the mic.
pub enum MicStream {
    Cpal(cpal::Stream),
    #[cfg(target_os = "windows")]
    Raw(wasapi_raw::RawMicCapture),
}

/// Starts the capture stream. Frames of exactly [`FRAME_SAMPLES`] mono i16
/// samples at 48 kHz arrive on `frames_tx` (20 ms cadence).
///
/// On Windows this first tries the WASAPI **raw** capture (bypasses the
/// system APOs — the Discord "Bypass System Audio Input Processing" equivalent),
/// because the OS/driver mic processing (gain boost, enhancements) can amplify
/// the noise floor. Falls back to cpal (shared mode, APOs applied) on any
/// error or when `LUMEN_NO_RAW_MIC` is set.
pub fn start_capture(
    frames_tx: tokio::sync::mpsc::UnboundedSender<Vec<i16>>,
) -> anyhow::Result<MicStream> {
    #[cfg(target_os = "windows")]
    {
        if std::env::var("LUMEN_NO_RAW_MIC").is_err() {
            match wasapi_raw::RawMicCapture::start(frames_tx.clone()) {
                Ok(raw) => {
                    eprintln!("lumen voice: using WASAPI raw mic capture (APO bypass)");
                    return Ok(MicStream::Raw(raw));
                }
                Err(e) => {
                    eprintln!("lumen voice: raw mic capture failed, falling back to cpal: {e}");
                }
            }
        }
    }
    Ok(MicStream::Cpal(cpal_capture(frames_tx)?))
}

/// cpal-based capture (WASAPI shared mode on Windows — APOs are applied).
fn cpal_capture(
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
// WASAPI raw-mode mic capture (Windows only) — the "Bypass System Audio Input
// Processing" equivalent. Windows/driver APOs (gain boost, enhancements) run
// on the mic signal in WASAPI shared mode; opening the stream with
// AUDCLNT_STREAMFLAGS_SYSTEM_MODE_RAW bypasses them, giving the denoiser the
// raw signal (which can otherwise arrive noise-boosted). Falls back to cpal.
// ---------------------------------------------------------------------------
#[cfg(target_os = "windows")]
mod wasapi_raw {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc::UnboundedSender;
    use windows::core::GUID;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    /// Raw-mode stream flag — not exposed by windows 0.62.2 (0x400).
    const AUDCLNT_STREAMFLAGS_SYSTEM_MODE_RAW: u32 = 0x0000_0400;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    // Shadow the crate's u32 WAVE_FORMAT_PCM (wFormatTag is u16).
    const WAVE_FORMAT_PCM: u16 = 1;
    const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;

    const CLSID_MMDEVICE_ENUMERATOR: GUID = GUID {
        data1: 0xBCDE0395,
        data2: 0xE52F,
        data3: 0x467C,
        data4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
    };
    const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID {
        data1: 0x00000001,
        data2: 0x0000,
        data3: 0x0010,
        data4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
    };
    const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID = GUID {
        data1: 0x00000003,
        data2: 0x0000,
        data3: 0x0010,
        data4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71],
    };

    #[derive(Clone, Copy, PartialEq)]
    enum Fmt {
        I16,
        F32,
    }

    /// Handle to the running raw WASAPI capture. Dropping it stops the thread.
    pub struct RawMicCapture {
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl RawMicCapture {
        /// Start the raw capture and push frames into `frames_tx`. Blocks until
        /// the WASAPI stream is running or fails.
        pub fn start(frames_tx: UnboundedSender<Vec<i16>>) -> anyhow::Result<Self> {
            let stop = Arc::new(AtomicBool::new(false));
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
            let stop2 = stop.clone();
            let frames_tx2 = frames_tx.clone();
            let thread = std::thread::Builder::new()
                .name("lumen-raw-mic".into())
                .spawn(move || run_raw_capture(frames_tx2, stop2, ready_tx))
                .map_err(|e| anyhow::anyhow!("spawn raw mic thread: {e}"))?;
            match ready_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => Ok(Self { stop, thread: Some(thread) }),
                Ok(Err(e)) => {
                    stop.store(true, Ordering::SeqCst);
                    let _ = thread.join();
                    anyhow::bail!("raw mic init failed: {e}")
                }
                Err(_) => {
                    stop.store(true, Ordering::SeqCst);
                    let _ = thread.join();
                    anyhow::bail!("raw mic init timed out")
                }
            }
        }
    }

    impl Drop for RawMicCapture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(h) = self.thread.take() {
                let _ = h.join();
            }
        }
    }

    /// Map a `windows::core::Result` to `anyhow::Result` (avoids depending on
    /// whether the error type implements `std::error::Error`).
    fn werr<T>(r: windows::core::Result<T>) -> anyhow::Result<T> {
        r.map_err(|e| anyhow::anyhow!("wasapi: {e}"))
    }

    fn run_raw_capture(
        frames_tx: UnboundedSender<Vec<i16>>,
        stop: Arc<AtomicBool>,
        ready_tx: std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        let result = setup_and_loop(&frames_tx, &stop, &ready_tx);
        let _ = ready_tx.send(result.map_err(|e| e.to_string()));
    }

    fn setup_and_loop(
        frames_tx: &UnboundedSender<Vec<i16>>,
        stop: &Arc<AtomicBool>,
        ready_tx: &std::sync::mpsc::Sender<Result<(), String>>,
    ) -> anyhow::Result<()> {
        unsafe {
            // COM (MTA) for WASAPI on this thread. A fresh thread inits cleanly;
            // if it somehow fails, the WASAPI calls below fail -> cpal fallback.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

            let enumerator: IMMDeviceEnumerator = werr(CoCreateInstance(
                &CLSID_MMDEVICE_ENUMERATOR,
                None,
                CLSCTX_ALL,
            ))?;
            let device = werr(enumerator.GetDefaultAudioEndpoint(eCapture, eConsole))?;
            let client: IAudioClient = werr(device.Activate(CLSCTX_ALL, None))?;

            let mix_ptr = werr(client.GetMixFormat())?;
            let (rate, channels, fmt) = parse_format(mix_ptr)?;

            let event: HANDLE = werr(CreateEventW(None, false, false, None))?;
            werr(client.SetEventHandle(event))?;

            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_SYSTEM_MODE_RAW;
            let hns_buffer = 200_000i64; // 20 ms
            werr(client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                hns_buffer,
                0,
                mix_ptr,
                None,
            ))?;
            CoTaskMemFree(Some(mix_ptr as *const core::ffi::c_void));

            let capture: IAudioCaptureClient = werr(client.GetService())?;
            werr(client.Start())?;

            // Signal ready (the capture loop is about to run).
            let _ = ready_tx.send(Ok(()));

            let mut resampler = LinearResampler::new(rate, CLOCK_RATE);
            let mut acc: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES);
            loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                if WaitForSingleObject(event, 50) != WAIT_OBJECT_0 {
                    continue; // timeout — keep polling the stop flag
                }
                // Drain all available buffers.
                loop {
                    let mut data: *mut u8 = std::ptr::null_mut();
                    let mut frames: u32 = 0;
                    let mut flags: u32 = 0;
                    if capture
                        .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                        .is_err()
                    {
                        break; // no more buffers ready
                    }
                    if frames > 0 {
                        if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 {
                            let zero = vec![0i16; frames as usize * channels];
                            feed(&mut resampler, &zero, channels, &mut acc, frames_tx);
                        } else if !data.is_null() {
                            feed_wasapi(
                                &mut resampler,
                                data,
                                frames as usize,
                                channels,
                                fmt,
                                &mut acc,
                                frames_tx,
                            );
                        }
                    }
                    let _ = capture.ReleaseBuffer(frames);
                }
            }
            let _ = client.Stop();
            let _ = CloseHandle(event);
            Ok(())
        }
    }

    fn parse_format(mix: *const WAVEFORMATEX) -> anyhow::Result<(u32, usize, Fmt)> {
        unsafe {
            // WAVEFORMATEX is `packed(1)`, so read each field unaligned via a
            // raw pointer (`addr_of!` + `read_unaligned`) — referencing a field
            // of a packed struct is E0793.
            let rate = std::ptr::addr_of!((*mix).nSamplesPerSec).read_unaligned();
            let channels = std::ptr::addr_of!((*mix).nChannels).read_unaligned() as usize;
            let tag = std::ptr::addr_of!((*mix).wFormatTag).read_unaligned();
            let bits = std::ptr::addr_of!((*mix).wBitsPerSample).read_unaligned();
            if channels == 0 || rate == 0 {
                anyhow::bail!("bad mix format: rate={rate} ch={channels}");
            }
            let fmt = if tag == WAVE_FORMAT_IEEE_FLOAT {
                Fmt::F32
            } else if tag == WAVE_FORMAT_PCM && bits == 16 {
                Fmt::I16
            } else if tag == WAVE_FORMAT_EXTENSIBLE {
                let sub = std::ptr::addr_of!(
                    (*(mix as *const WAVEFORMATEXTENSIBLE)).SubFormat
                )
                .read_unaligned();
                if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                    Fmt::F32
                } else if sub == KSDATAFORMAT_SUBTYPE_PCM && bits == 16 {
                    Fmt::I16
                } else {
                    anyhow::bail!("unsupported extensible mic format ({bits})");
                }
            } else {
                anyhow::bail!("unsupported mic format tag {tag}");
            };
            Ok((rate, channels, fmt))
        }
    }

    fn feed_wasapi(
        resampler: &mut LinearResampler,
        data: *mut u8,
        frames: usize,
        channels: usize,
        fmt: Fmt,
        acc: &mut Vec<i16>,
        frames_tx: &UnboundedSender<Vec<i16>>,
    ) {
        match fmt {
            Fmt::I16 => {
                let samples =
                    unsafe { std::slice::from_raw_parts(data as *const i16, frames * channels) };
                feed(resampler, samples, channels, acc, frames_tx);
            }
            Fmt::F32 => {
                let samples =
                    unsafe { std::slice::from_raw_parts(data as *const f32, frames * channels) };
                let mut s16: Vec<i16> = Vec::with_capacity(samples.len());
                for &v in samples {
                    s16.push((v * 32767.0) as i16);
                }
                feed(resampler, &s16, channels, acc, frames_tx);
            }
        }
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
    /// RNNoise denoiser — the LIGHT fallback (used only if DeepFilterNet can't
    /// load). Full-band 48 kHz.
    rnnoise: Option<Box<nnnoiseless::DenoiseState<'static>>>,
    /// DeepFilterNet3 (full-band 48 kHz, tract) — the primary denoiser.
    /// Measured ~4-5% of a core with full bandwidth — cheaper and better than
    /// the old GTCRN. `None` means the model failed to load and we fall back
    /// to RNNoise.
    neural: Option<DeepFilterDenoiser>,
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
        // DeepFilterNet3 by default (full-band, ~4-5% of a core). If the model
        // fails to load, falls back to the RNNoise light path. No runtime tier
        // switching: DeepFilterNet is cheaper than RNNoise, so there is no
        // cheaper tier to degrade to under load.
        Self::chain(DeepFilterDenoiser::new())
    }

    /// Construct the send-path DSP in LIGHT mode only (WebRTC AEC3/NS +
    /// RNNoise + leveler, no DeepFilterNet). Used by probes/tests.
    pub fn new_light() -> Self {
        Self::chain(None)
    }

    /// Shared construction: WebRTC APM (AEC3 + HPF) + leveler, plus the
    /// optional DeepFilterNet denoiser. Classic WebRTC NS is ON only in the
    /// light (RNNoise) path: DeepFilterNet is the denoiser, and running NS
    /// before it double-colors the speech. AGC is OFF in both (measured: the
    /// fixed-digital AGC amplified background noise before NS removed it).
    fn chain(neural: Option<DeepFilterDenoiser>) -> Self {
        let processor = Processor::new(CLOCK_RATE).ok().map(|processor| {
            processor.set_config(apm_config(neural.is_some()));
            processor
        });
        Self {
            processor,
            rnnoise: Some(nnnoiseless::DenoiseState::new()),
            neural,
            leveler: SpeechLeveler::new(),
            speech_detected: false,
            gate_hangover: 0,
        }
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
        if let Some(n) = self.neural.as_mut() {
            // DeepFilterNet: the primary denoiser. It silences noise to ~0
            // (measured ~163 dB on white noise), so the post-denoise energy IS
            // the speech detector — no extra VAD, no extra CPU. Map it to a
            // VAD probability for the leveler.
            result = n.process(&out);
            let r = rms_level(&result);
            vad = (r / SPEECH_ENERGY_REF).clamp(0.0, 1.0);
            rms = r;
        } else {
            // Light fallback: RNNoise denoising and its VAD — full-band.
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

    /// Send-path entry point used by the live mic path. ALWAYS runs the full
    /// denoise chain so DeepFilterNet's streaming state stays warm (skipping
    /// it on silence chops speech at re-entry), and returns `Some(cleaned)`
    /// when the frame should be transmitted, `None` when it's quiet (past the
    /// hangover) and the caller should skip encode + transmit. DeepFilterNet
    /// is cheap enough that always processing costs little.
    ///
    /// ```ignore
    /// while let Some(frame) = mic.recv().await {
    ///     if let Some(cleaned) = ns.process_gated(&frame) {
    ///         let pkt = encode(&cleaned);
    ///         for peer in peers { peer.send(pkt.clone()); }
    ///     }
    ///     // None => quiet: denoised but not transmitted.
    /// }
    /// ```
    pub fn process_gated(&mut self, frame: &[i16]) -> Option<Vec<i16>> {
        // Always run the full chain (AEC3 + DeepFilterNet) so the streaming
        // denoiser's lookahead buffers stay warm. Skipping the denoiser on
        // silence (the old gate) let its state go stale, which chopped speech
        // at every re-entry (the reported "entrecortada"). DeepFilterNet is
        // cheap (~4% of a core), so always processing costs little.
        let cleaned = self.process(frame);

        // The energy gate now only decides whether to TRANSMIT, not whether to
        // denoise: quiet frames (past the hangover) are denoised but not sent.
        // The far side hears silence, and the denoiser stays coherent across
        // speech re-entry. The hangover keeps transmitting speech tails so
        // word endings aren't clipped.
        let level = rms_level(frame);
        if level >= VOICE_ENERGY_FLOOR {
            self.gate_hangover = GATE_HANGOVER_FRAMES;
        } else if self.gate_hangover > 0 {
            self.gate_hangover -= 1;
        } else {
            return None; // quiet: denoised but not transmitted
        }
        Some(cleaned)
    }

    /// Whether the most recently processed frame contained speech (RNNoise
    /// VAD). Drives voice-activation (transmit gating) and the speaking meter.
    pub fn speech_detected(&self) -> bool {
        self.speech_detected
    }

    /// Whether the DeepFilterNet denoiser is available (loaded). `false` means
    /// the model failed to load and the chain uses the RNNoise fallback.
    pub fn neural_available(&self) -> bool {
        self.neural.is_some()
    }
}

/// Build the WebRTC APM config for a given denoiser tier. AEC3 + high-pass are
/// always on; WebRTC NS (VeryHigh, ~9x stationary-noise attenuation) is ON in
/// the light tier (RNNoise needs it) and OFF in the neural tier (DeepFilterNet is the
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
// DeepFilterDenoiser (DeepFilterNet3, tract) — the primary full-band denoiser
// ---------------------------------------------------------------------------

/// DeepFilterNet3 speech-enhancement denoiser — the Krisp-like full-band
/// (48 kHz) tier. Runs via `tract` (pure-Rust ONNX, no onnxruntime — no C
/// symbol collision). Measured ~4-5% of a core (RTF ~0.04) on a 2014 i5 with
/// full bandwidth — strictly cheaper AND better than the old GTCRN (16 kHz
/// band-limited to ~8 kHz, ~23% of a core). Stateful streaming with a ~30 ms
/// lookahead delay. Replaces GTCRN; DPDFNet (the other full-band option) is
/// blocked by a sherpa-onnx incompatibility, so this is the working choice.
pub struct DeepFilterDenoiser {
    model: df::tract::DfTract,
}

impl DeepFilterDenoiser {
    /// Create from the embedded DeepFilterNet3 model. `None` if it can't load
    /// (e.g. tract init failed) — the caller then falls back to RNNoise.
    pub fn new() -> Option<Self> {
        use df::tract::{DfParams, DfTract, RuntimeParams};
        match DfTract::new(DfParams::default(), &RuntimeParams::default()) {
            Ok(model) => Some(Self { model }),
            Err(e) => {
                eprintln!("lumen voice: DeepFilterNet load failed: {e}");
                None
            }
        }
    }

    /// Denoise a 48 kHz mono frame. DeepFilterNet runs at 48 kHz with a 10 ms
    /// (480-sample) hop, so a 20 ms (960-sample) frame is fed as two chunks and
    /// produces 960 samples out. The model is stateful with a ~30 ms
    /// lookahead delay (inherent to streaming enhancement — the caller's
    /// jitter buffer absorbs it).
    pub fn process(&mut self, frame: &[i16]) -> Vec<i16> {
        let hop = self.model.hop_size;
        let mut result = Vec::with_capacity(frame.len());
        let mut in_arr = ndarray::Array2::<f32>::zeros((1, hop));
        let mut out_arr = ndarray::Array2::<f32>::zeros((1, hop));
        for chunk in frame.chunks_exact(hop) {
            for (i, &s) in chunk.iter().enumerate() {
                in_arr[[0, i]] = s as f32 / 32768.0;
            }
            let _ = self.model.process(in_arr.view(), out_arr.view_mut());
            for i in 0..hop {
                let v = out_arr[[0, i]];
                result.push(
                    (v * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16,
                );
            }
        }
        // Pad a trailing partial chunk (shouldn't happen at 960/480).
        while result.len() < frame.len() {
            result.push(0);
        }
        result
    }
}

// SAFETY: `DeepFilterDenoiser` holds a `df::tract::DfTract`, which is not
// `Send` because its STFT frontend keeps an `Arc<dyn RealToComplex>` trait
// object (realfft doesn't mark the trait `Send`). The denoiser is only ever
// owned and used by the single send-task thread (it lives inside the
// `NoiseSuppressor` that the send task creates and never shares), so handing
// it across the `tokio::spawn` boundary is sound.
unsafe impl Send for DeepFilterDenoiser {}

// ---------------------------------------------------------------------------
// Speech leveler: VAD-gated adaptive gain
// ---------------------------------------------------------------------------
/// VAD-gated adaptive gain (a sidechain compressor): raises quiet speech to a
/// target level and gates silence, so a cheap/quiet mic is audible WITHOUT
/// amplifying background noise.
///
/// WebRTC's adaptive AGC (GainController2) was measured to boost the noise
/// floor (+15 dB on quiet noise), so we do the gate ourselves: the VAD signal
/// (post-denoise energy in the DeepFilterNet path, RNNoise VAD in the fallback)
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
/// denoise + Opus + transmit). A normal speaking voice is ~RMS 0.1 (well above);
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
        let mut ns = NoiseSuppressor { processor: Some(processor), rnnoise: None, neural: None, leveler: SpeechLeveler::new(), speech_detected: false, gate_hangover: 0 };
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
    fn deepfilter_streaming_no_dropped_frames() {
        // The streaming denoiser must keep output 20 ms aligned with no
        // silence-padded frames once warm, and the speech energy must survive
        // (not be gated away). Use a speech-like AM signal.
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
        let Some(mut g) = DeepFilterDenoiser::new() else { return };
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
    fn deepfilter_path_speech_detection_by_energy() {
        // The DeepFilterNet path derives speech detection from post-denoise
        // energy (no separate neural VAD): the denoiser silences noise to ~0,
        // so energy implies speech. Noise must not open the gate; speech must.
        if DeepFilterDenoiser::new().is_none() {
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
        // Speech: must be detected (post-denoise energy above the threshold).
        let mut ns = NoiseSuppressor::new();
        let mut speech_detected = false;
        for i in 0..40 {
            ns.process(&speech(i));
            speech_detected |= ns.speech_detected();
        }
        assert!(speech_detected, "speech must be detected via post-denoise energy");
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



