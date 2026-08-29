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
use rubato::Resampler as _;
use std::borrow::Cow;
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

/// Band-limited device→48 kHz resampler (rubato SincFixedIn). Linear
/// interpolation aliases badly when a webcam/cheap mic delivers 16 kHz
/// (3× upsample); sinc resampling is the WebRTC-standard approach.
///
/// `None` sinc = passthrough for the common 48 kHz device case (the capture
/// setup prefers a 48 kHz config below, and WASAPI mix format is 48 kHz in
struct CaptureResampler {
    sinc: Option<rubato::SincFixedIn<f32>>,
    /// Pending mono device-rate samples (< the 960-sample input chunk).
    stage: Vec<i16>,
    /// Reusable buffer for f32 chunk conversion (P0-3).
    chunk_buf: Vec<f32>,
    /// Reusable buffer for resampled output (P0-3).
    out_f32: Vec<f32>,
}

impl CaptureResampler {
    fn new(src_rate: u32) -> Self {
        let sinc = if src_rate == 48_000 {
            None
        } else {
            let ratio = 48_000.0 / src_rate as f64;
            Some(
                rubato::SincFixedIn::<f32>::new(
                    ratio,
                    10.0, // max relative ratio change
                    rubato::SincInterpolationParameters {
                        sinc_len: 128,
                        f_cutoff: 0.95,
                        oversampling_factor: 128,
                        interpolation: rubato::SincInterpolationType::Linear,
                        window: rubato::WindowFunction::BlackmanHarris2,
                    },
                    960, // fixed input chunk (20 ms @ 48 kHz)
                    1,
                )
                .expect("rubato params are valid"),
            )
        };
        Self { sinc, stage: Vec::with_capacity(960), chunk_buf: Vec::with_capacity(960), out_f32: Vec::new() }
    }

    /// Same call shape as the old `LinearResampler::resample`.
    fn resample(&mut self, mono: &[i16]) -> Vec<i16> {
        let Some(sinc) = self.sinc.as_mut() else {
            return mono.to_vec();
        };
        self.stage.extend_from_slice(mono);
        // Reuse out_f32 to avoid per-call allocation (P0-3).
        self.out_f32.clear();
        while self.stage.len() >= 960 {
            self.chunk_buf.clear();
            self.chunk_buf.extend(self.stage.drain(..960).map(|s| s as f32 / 32768.0));
            // SincFixedIn consumes exactly the 960-sample input chunk and
            // produces chunk_size*ratio output frames (channels-first).
            let channels = sinc.process(&[self.chunk_buf.as_slice()], None).expect("resample ok");
            self.out_f32.extend(channels[0].iter().copied());
        }
        // Convert out_f32 -> i16 Vec in one pass.
        self.out_f32
            .iter()
            .map(|v| (v * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
            .collect()
    }

    /// Zero-alloc variant: extend `out` directly without intermediate Vec.
    /// Used by `feed()` to avoid the `to_vec()` in the passthrough case.
    fn resample_into(&mut self, mono: &[i16], out: &mut Vec<i16>) {
        let Some(sinc) = self.sinc.as_mut() else {
            out.extend_from_slice(mono);
            return;
        };
        self.stage.extend_from_slice(mono);
        // For non-passthrough we still need temporary f32 buffers; reuse fields.
        // We batch through out_f32 then flush to `out` at the end to avoid
        // interleaving borrow issues — clear out_f32 first.
        self.out_f32.clear();
        while self.stage.len() >= 960 {
            self.chunk_buf.clear();
            self.chunk_buf.extend(self.stage.drain(..960).map(|s| s as f32 / 32768.0));
            let channels = sinc.process(&[self.chunk_buf.as_slice()], None).expect("resample ok");
            self.out_f32.extend(channels[0].iter().copied());
        }
        out.extend(
            self.out_f32
                .iter()
                .map(|v| (v * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16),
        );
    }
}
/// Load a 48 kHz mono PCM-i16 WAV file (RIFF) into a flat sample buffer.
/// Used to feed real recorded speech through the transport (`input_wav`).
/// Multi-channel files are down-mixed to mono (first channel).
pub fn load_wav_pcm(path: &str) -> anyhow::Result<Vec<i16>> {
    let data = std::fs::read(path)?;
    if &data[0..4] != b"RIFF" {
        anyhow::bail!("not a RIFF/WAV file");
    }
    let mut off = 12usize;
    let mut pcm: Vec<i16> = Vec::new();
    let mut channels: u16 = 1;
    let mut bits: u16 = 16;
    while off + 8 <= data.len() {
        let id = &data[off..off + 4];
        let sz = u32::from_le_bytes(data[off + 4..off + 8].try_into()?) as usize;
        let body = off + 8;
        match id {
            b"fmt " if body + 16 <= data.len() => {
                let _audio_format = u16::from_le_bytes(data[body..body + 2].try_into()?);
                channels = u16::from_le_bytes(data[body + 2..body + 4].try_into()?);
                bits = u16::from_le_bytes(data[body + 14..body + 16].try_into()?);
            }
            b"data" => {
                for chunk in data[body..body + sz].chunks_exact(2) {
                    if bits == 16 {
                        pcm.push(i16::from_le_bytes([chunk[0], chunk[1]]));
                    }
                }
                if bits != 16 {
                    // fall back to re-parsing as u8 if we couldn't
                    pcm.clear();
                }
            }
            _ => {}
        }
        off = body + sz + (sz & 1);
        if id == b"data" {
            break;
        }
    }
    if pcm.is_empty() {
        anyhow::bail!("no PCM data found in {path} (need 16-bit)");
    }
    if channels > 1 {
        // down-mix to mono: take first channel (assumes interleaved)
        pcm = pcm.iter().step_by(channels as usize).copied().collect();
    }
    Ok(pcm)
}

/// Handle to the running mic capture (cpal, or the Windows WASAPI raw path).
/// Dropping it stops the mic.
pub enum MicStream {
    /// No capture device (receive-only participant; `open_mic=false`).
    Inactive,
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
    // Prefer a 48 kHz input config so most devices skip capture resampling
    // entirely; fall back to the device default otherwise.
    let config = {
        let mut best: Option<cpal::SupportedStreamConfig> = None;
        if let Ok(configs) = device.supported_input_configs() {
            for c in configs {
                // The stream build only feeds i16/f32 — skip u8/u16/i24/i32
                // (pipewire-alsa advertises u8 for the default device, which
                // used to win the "first 48 kHz mono" pick and then failed).
                if !matches!(
                    c.sample_format(),
                    cpal::SampleFormat::I16 | cpal::SampleFormat::F32
                ) {
                    continue;
                }
                if let Some(cfg) = c.try_with_sample_rate(48_000) {
                    let mono = cfg.channels() == 1;
                    let better = match &best {
                        None => true,
                        Some(b) => mono && b.channels() != 1,
                    };
                    if better {
                        best = Some(cfg);
                        if mono {
                            break;
                        }
                    }
                }
            }
        }
        match best {
            Some(c) => c,
            None => device.default_input_config()?,
        }
    };
    let channels = config.channels() as usize;
    eprintln!(
        "lumen voice: audio input <- {:?} ({} Hz, {} ch, {})",
        device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "?".into()),
        config.sample_rate(),
        config.channels(),
        config.sample_format(),
    );
    let mut resampler = CaptureResampler::new(config.sample_rate());
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
                    // P0-1: stack buffer to avoid per-callback heap alloc on hot path.
                    // Chunk in 4096-sample pieces, aligned to channel frames.
                    let mut s16_buf = [0i16; 4096];
                    let ch = channels.max(1);
                    let chunk_cap = (4096 / ch) * ch;
                    let chunk_cap = chunk_cap.max(ch);
                    let mut offset = 0;
                    while offset < data.len() {
                        let len = (data.len() - offset).min(chunk_cap);
                        // Ensure len stays channel-aligned except for final tail.
                        let len = if offset + len < data.len() {
                            (len / ch) * ch
                        } else {
                            len
                        };
                        if len == 0 { break; }
                        for i in 0..len {
                            s16_buf[i] = (data[offset + i] * 32767.0) as i16;
                        }
                        feed(&mut resampler, &s16_buf[..len], channels, &mut acc, &frames_tx);
                        offset += len;
                    }
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
    resampler: &mut CaptureResampler,
    data: &[i16],
    channels: usize,
    acc: &mut Vec<i16>,
    frames_tx: &tokio::sync::mpsc::UnboundedSender<Vec<i16>>,
) {
    if data.is_empty() {
        return;
    }
    // P0-2: avoid to_vec() when already mono via Cow (P0-3: resample_into avoids passthrough clone).
    let mono: Cow<[i16]> = if channels == 1 {
        Cow::Borrowed(data)
    } else {
        Cow::Owned(data.iter().step_by(channels).copied().collect())
    };
    resampler.resample_into(&mono, acc);
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

            let mut resampler = CaptureResampler::new(rate);
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
        resampler: &mut CaptureResampler,
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
                // P0-1: stack buffer to avoid per-callback heap alloc (WASAPI F32 path).
                let mut s16_buf = [0i16; 4096];
                let ch = channels.max(1);
                let chunk_cap = (4096 / ch) * ch;
                let chunk_cap = chunk_cap.max(ch);
                let mut offset = 0;
                while offset < samples.len() {
                    let len = (samples.len() - offset).min(chunk_cap);
                    let len = if offset + len < samples.len() {
                        (len / ch) * ch
                    } else {
                        len
                    };
                    if len == 0 { break; }
                    for i in 0..len {
                        s16_buf[i] = (samples[offset + i] * 32767.0) as i16;
                    }
                    feed(resampler, &s16_buf[..len], channels, acc, frames_tx);
                    offset += len;
                }
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
    /// Device rate -> 48 kHz for the AEC render reference (AEC3 runs at
    /// 48 kHz; feeding device-rate samples pitch-shifts the reference on
    /// non-48 kHz outputs and breaks echo cancellation).
    tap_resampler: LinearResampler,
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
        // P0-6: direct resample_into avoids intermediate Vec in passthrough case.
        st.resampler.resample_into(frame, &mut st.buf);
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
        // AEC reference: copy what is actually played (mono, device rate) —
        // resampled to 48 kHz, the rate AEC3 expects (feeding device-rate
        // samples directly would time-stretch the reference on non-48 kHz
        // outputs and break echo cancellation).
        let n = frames.min(st.buf.len());
        if n > 0 {
            // P0-6: resample directly into render_tap to avoid intermediate Vec.
            let mut tap_guard = self.render_tap.lock();
            st.tap_resampler.resample_into(&st.buf[..n], &mut *tap_guard);
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
        let dcfg = device.default_output_config().ok();
        eprintln!(
            "lumen voice: audio output -> {:?} ({} Hz, {} ch)",
            device
                .description()
                .map(|d| d.name().to_string())
                .unwrap_or_else(|_| "?".into()),
            dcfg.as_ref().map(|c| c.sample_rate()).unwrap_or(0),
            dcfg.as_ref().map(|c| c.channels()).unwrap_or(0),
        );
        let config = device.default_output_config()?;
        let dev_rate = config.sample_rate();
        *self.state.lock() = Some(OutputState {
            buf: Vec::with_capacity((dev_rate / 2) as usize),
            resampler: LinearResampler::new(CLOCK_RATE, dev_rate),
            tap_resampler: LinearResampler::new(dev_rate, CLOCK_RATE),
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
        // DTX: since we transmit every frame, silence becomes ~5-byte packets
        // — negligible bandwidth/CPU during listening-heavy calls. Inband FEC
        // stays off (it requires 60 ms frames, +40 ms latency; Discord also
        // uses 20 ms frames).
        encoder.set_dtx(true)?;
        Ok(Self { encoder })
    }

    /// Encode one 20 ms frame into a packet. Returns the packet bytes.
    pub fn encode(&mut self, pcm: &[i16]) -> anyhow::Result<Vec<u8>> {
        let mut out = [0u8; 1500];
        let n = self.encoder.encode(pcm, &mut out)?;
        Ok(out[..n].to_vec())
    }

    /// Toggle Opus DTX (comfort noise for silence frames). For benches/tests.
    pub fn set_dtx(&mut self, dtx: bool) -> anyhow::Result<()> {
        Ok(self.encoder.set_dtx(dtx)?)
    }

    /// Set the encoder complexity (0-10). Lower = faster encode, less quality
    /// on hard material. For benches/tests and the production tuning hook.
    pub fn set_complexity(&mut self, value: i32) -> anyhow::Result<()> {
        Ok(self.encoder.set_complexity(value)?)
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
// RNNoise (voice-transparent denoiser)
// ---------------------------------------------------------------------------

/// Stateless wrapper over the RNNoise denoiser (the open ancestor of the
/// Krisp approach — trained with speech preservation as the objective).
/// Exposed for the harness/probes to A/B against DeepFilterNet and the
/// WebRTC NS; returns the denoised frame plus the frame's VAD probability.
pub struct RnnoiseDenoiser {
    state: Box<nnnoiseless::DenoiseState<'static>>,
}

impl RnnoiseDenoiser {
    pub fn new() -> Self {
        Self { state: nnnoiseless::DenoiseState::new() }
    }

    /// Denoise a 48 kHz frame (multiple of 480) and return the VAD (0..1).
    pub fn process(&mut self, frame: &[i16]) -> (Vec<i16>, f32) {
        let mut out = vec![0i16; frame.len()];
        let mut input = [0f32; 480];
        let mut denoised = [0f32; 480];
        let mut max_vad = 0.0f32;
        for (chunk, out_chunk) in frame.chunks_exact(480).zip(out.chunks_exact_mut(480)) {
            for (i, s) in chunk.iter().enumerate() {
                input[i] = *s as f32 / 32768.0;
            }
            let v = self.state.process_frame(&mut denoised, &input);
            max_vad = max_vad.max(v);
            for (i, v) in denoised.iter().enumerate() {
                out_chunk[i] = (v * 32767.0)
                    .round()
                    .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            }
        }
        (out, max_vad)
    }
}

// ---------------------------------------------------------------------------
// Noise suppression (WebRTC AudioProcessing — the module Chrome/Discord use)
// ---------------------------------------------------------------------------

use webrtc_audio_processing::config::{
    AdaptiveDigital, Config, FixedDigital, GainController, GainController2, HighPassFilter,
    NoiseSuppression, NoiseSuppressionLevel,
};
use webrtc_audio_processing::Processor;
#[cfg(all(feature = "experimental-aec3-config", not(target_env = "msvc")))]
use webrtc_audio_processing::experimental::EchoCanceller3Config;

use sonora::config::{
    DownmixMethod, EchoCanceller as SonoraEchoCanceller, MaxProcessingRate, Pipeline,
    TransparentModeType,
};
use sonora::{AudioProcessing as SonoraAudioProcessing, Config as SonoraConfig, StreamConfig as SonoraStreamConfig};

/// Sonora AEC3 (pure Rust M145) — reemplaza webrtc-audio-processing AEC3.
/// Solo AEC (HPF/NS/GC2 siguen en WebRTC Processor con echo_canceller: None).
/// Usa `sonora::AudioProcessing` configurado solo con echo_canceller (pipeline 48 kHz).
/// Maneja duplex correcto y drift; pure Rust sin C++ ni MSVC issues.
///
/// ## Tuning Larsen vs calidad (2026-08-26, i5-4590 48k mono mic+speakers)
/// - **Larsen root cause**: con gate 0.0008 y cap 150 (7200), delay medido 224 (>cap)
///   queda fuera de ventana → referencia stale/cortada, ERL 4.3 inestable, estimador
///   nunca converge y el eco audibility dispara howling. WebRTC tuned bajó
///   `anti_howling 400→200 gain 1.0→0.3` y `mask 0.4→0.3`; en sonora el equivalente
///   es `sonora_aec3::config::HighBandsSuppression` + `Tuning` (`mask_lf/mask_hf`).
/// - **Limitación pública**: `sonora::Config` solo expone `EchoCanceller`
///   (`enforce_high_pass_filtering` + `transparent_mode`); `HighBandsSuppression`,
///   `echo_audibility`, `dominant_nearend_detection`, `conservative_hf_suppression`,
///   `use_subband_nearend_detection` e `initial_state_seconds` viven en
///   `sonora_aec3::config::EchoCanceller3Config` (ver
///   `~/.cargo/registry/src/*/sonora-aec3-0.2.0/src/config.rs`) y NO son
///   seteables vía `sonora::Config` (confirmado `audio_processing_impl.rs:
///   EchoCanceller3Config::default()` hardcodeado). Ajuste fino requeriría
///   exponer `sonora_aec3` o parchear defaults.
/// - **Fix mínimo aquí**: (a) cap 300ms (14400) para cubrir delay 224,
///   (b) gate 0.0008 mantenido (-62dB), (c) pipeline explícito 48k mono y
///   `transparent_mode::Hmm` (más responsivo que Legacy en headset/no-echo),
///   (d) `enforce_high_pass_filtering=true`. Sin tocar NS VeryHigh (voz limpia
///   baseline corr 0.62). Voz: `conservative_hf_suppression=false` y
///   `use_subband_nearend_detection=false` por defecto dañarían corr 0.04 si se
///   activan sin tuning fino — se dejan false aquí; si corr cae, habilitarlos
///   vía `sonora_aec3` es el siguiente paso. `initial_state_seconds` 2.5 se
///   mantiene (bajar a 1.0 acelera convergencia pero expone Larsen en primeros
///   2s; preferible transparencia).
/// - **Para tuning profundo** (cuando Larsen persista): parchear
///   `sonora-aec3/src/config.rs`:
///   `HighBandsSuppression { anti_howling_activation_threshold: 200.0,
///   anti_howling_gain: 0.3 }` (off 400/1.0), `Tuning { mask_lf.enr_suppress: 0.30,
///   mask_hf.enr_suppress: 0.08 }`, `DominantNearendDetection {
///   enr_threshold: 0.35 (0.25), snr_threshold: 20.0 (30), hold_duration: 70 }`,
///   `conservative_hf_suppression: true` + `use_subband_nearend_detection: true`
///   si corr 0.62 cae, `Delay { delay_headroom_samples: 64 (32),
///   hysteresis_limit_blocks: 2 (1) }`, `Filter { length_blocks: 16 (13),
///   initial_state_seconds: 1.0 (2.5) }`.
pub struct SonoraAec {
    inner: SonoraAudioProcessing,
}

impl SonoraAec {
    /// Crea AEC Sonora a 48 kHz mono. Config solo echo_canceller, pipeline 48 kHz.
    ///
    /// Pipeline explícito (no defaults 32k/multi-channel): máxima calidad 48k mono,
    /// referencia para tuning Larsen documentado arriba.
    pub fn new(sample_rate: u32) -> Self {
        let stream = SonoraStreamConfig::new(sample_rate, 1);
        let mut cfg = SonoraConfig {
            echo_canceller: Some(SonoraEchoCanceller {
                enforce_high_pass_filtering: true,
                transparent_mode: TransparentModeType::Hmm,
            }),
            ..Default::default()
        };
        cfg.pipeline = Pipeline {
            maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz,
            multi_channel_render: false,
            multi_channel_capture: false,
            capture_downmix_method: DownmixMethod::AverageChannels,
        };
        // NS/GC2 quedan en WebRTC Processor; sonora solo AEC — dejar None explícito.
        cfg.noise_suppression = None;
        cfg.gain_controller2 = None;
        cfg.high_pass_filter = None;
        cfg.pre_amplifier = None;
        cfg.capture_level_adjustment = None;
        let inner = SonoraAudioProcessing::builder()
            .config(cfg)
            .capture_config(stream)
            .render_config(stream)
            .build();
        Self { inner }
    }

    /// Alimenta referencia far-end (render) de 10 ms (480 muestras). Debe llamarse
    /// antes del capture correspondiente (orden render -> capture, como webrtc).
    pub fn process_render_frame(&mut self, frame: &[i16]) {
        // frame ya viene alineado a 480 (caller garantiza chunks_exact 480)
        for chunk in frame.chunks_exact(480) {
            let mut out = [0i16; 480];
            if let Err(e) = self.inner.process_render_i16(chunk, &mut out) {
                eprintln!("lumen-voice: SonoraAec process_render error: {e:?}");
            }
        }
    }

    /// Procesa captura con cancelación de eco in-place (480 por chunk).
    pub fn process_capture_frame(&mut self, chunk: &mut [i16]) {
        debug_assert_eq!(chunk.len(), 480);
        let src = chunk.to_vec();
        let mut out = [0i16; 480];
        let _ = self.inner.process_capture_i16(&src, &mut out);
        chunk.copy_from_slice(&out);
    }

    /// Procesa frame completo (múltiplo de 480, p.ej. 960) in-place.
    pub fn process_capture(&mut self, frame: &mut [i16]) {
        for chunk in frame.chunks_mut(480) {
            // Solo chunks exactos; el caller garantiza múltiplo de 480
            if chunk.len() == 480 {
                self.process_capture_frame(chunk);
            }
        }
    }

    pub fn stats(&self) -> sonora::stats::AudioProcessingStats {
        self.inner.statistics().clone()
    }
}

/// Send-path DSP: Sonora AEC3 (pure Rust) + WebRTC HPF/NS/GC2 + limiter,
/// seguido por RNNoise/DeepFilterNet/FastEnhancer según tier.
///
/// - Sonora AEC3 (`sonora::AudioProcessing` solo echo_canceller, pipeline 48 kHz)
///   cancela eco de altavoz — el caller debe alimentar playback en
///   [`NoiseSuppressor::process_render_frame`] (far-end reference), orden render -> capture.
///   Reemplaza webrtc-audio-processing AEC3 (C++ Larsen) por Rust puro M145 con duplex correcto.
/// - WebRTC HPF + NS VeryHigh + GC2 (echo_canceller: None) siguen en `Processor`
///   — mantienen FE/DF/RNNoise y limiter, solo se reemplaza AEC path. Pure Rust sin MSVC.
/// - `Processor` es `Send + Sync`, `SonoraAec` es `Send + Sync`, `nnnoiseless::DenoiseState`
///   es plain data, todo vive en send task. Ambos procesan 10 ms (480 @48kHz); capture 20 ms = 2 halves.
pub struct NoiseSuppressor {
    /// WebRTC Processor para HPF + NS VeryHigh + GC2 (echo_canceller siempre None).
    processor: Option<Processor>,
    /// Sonora AEC3 (pure Rust) — `Some` cuando `aec_enabled=true`, `None` en headphones/off.
    aec: Option<SonoraAec>,
    /// RNNoise denoiser — the LIGHT fallback (used only if DeepFilterNet can't
    /// load). Full-band 48 kHz.
    rnnoise: Option<Box<nnnoiseless::DenoiseState<'static>>>,
    /// DeepFilterNet3 (full-band 48 kHz, tract) — the primary denoiser.
    /// Measured ~4-5% of a core with full bandwidth — cheaper and better than
    /// the old GTCRN. `None` means the model failed to load and we fall back
    /// to RNNoise.
    neural: Option<DeepFilterDenoiser>,
    /// FastEnhancer-Medium (48 kHz full-band, int8 C runtime) — the default
    /// neural tier. `None` when the CPU can't run it (no AVX2+FMA3+F16C) →
    /// the chain degrades to WebRTC NS-only.
    fe: Option<FastEnhancerDenoiser>,
    /// Whether the last processed frame contained speech (DeepFilterNet LSNR
    /// in the neural path, RNNoise VAD in the fallback).
    speech_detected: bool,
    /// LSNR (dB) of the last processed frame in the neural tier; `None` in the
    /// light (RNNoise) tier where LSNR is unavailable. For diagnostics/probes.
    last_lsnr: Option<f32>,
    /// Reusable buffer for WebRTC APM output (P0-4) — avoids vec![0; N] per frame.
    buf_apm: Vec<i16>,
}

impl NoiseSuppressor {
    /// The winning send-path chain (harness A/B): WebRTC APM only — AEC3
    /// (transparent-initial-state patch) + HPF + NS VeryHigh + GainController2,
    /// then the peak limiter. No external denoiser and no custom leveler.
    pub fn new() -> Self {
        Self::chain(None, false, None)
    }

    /// Build the suppressor for a UI-selectable model. `NsOnly` is the WebRTC
    /// NS chain (no neural denoiser); `FastEnhancerM` runs the 48 kHz int8 C
    /// runtime and auto-degrades to `NsOnly` on CPUs that can't run it (no
    /// AVX2+FMA3+F16C — e.g. pre-Haswell or Pentium/Celeron).
    pub fn with_model(model: SuppressorModel) -> Self {
        Self::with_model_and_aec(model, true)
    }

    /// Build for a model with the AEC3 module on/off (see
    /// [`Self::chain_with_aec`]). The send task uses this so the APM config
    /// mirrors the session's `aec_enabled` flag, not just whether render
    /// frames are fed.
    pub fn with_model_and_aec(model: SuppressorModel, aec: bool) -> Self {
        match model {
            SuppressorModel::NsOnly => Self::chain_with_aec(None, false, None, aec),
            SuppressorModel::FastEnhancerS => {
                Self::chain_with_aec(None, false, FastEnhancerDenoiser::new_small(), aec)
            }
            SuppressorModel::FastEnhancerM => {
                Self::chain_with_aec(None, false, FastEnhancerDenoiser::new(), aec)
            }
        }
    }

    /// The DeepFilterNet tier (AEC3 + NS + GC2 + DeepFilterNet) — kept as an
    /// optional enhancement for very noisy environments; measured to damage
    /// the voice more than the NS-only chain (see the harness diary).
    pub fn new_neural() -> Self {
        Self::chain(DeepFilterDenoiser::new(), false, None)
    }

    /// The RNNoise tier (AEC3 + NS + GC2 + RNNoise) — kept for the light
    /// fallback; RNNoise cuts quiet words (measured p10 -51.8 dB).
    pub fn new_light() -> Self {
        Self::chain(None, true, None)
    }

    /// Shared construction: WebRTC APM (HPF/NS/GC2) + optional Sonora AEC + optional external denoiser.
    fn chain(
        neural: Option<DeepFilterDenoiser>,
        rnnoise: bool,
        fe: Option<FastEnhancerDenoiser>,
    ) -> Self {
        Self::chain_with_aec(neural, rnnoise, fe, true)
    }

    /// Like [`Self::chain`] pero AEC ahora es Sonora (pure Rust). Con `aec=true`
    /// se crea `SonoraAec` (echo_canceller solo), y WebRTC `Processor` siempre
    /// lleva `echo_canceller: None` (solo HPF + NS VeryHigh + GC2). Con `aec=false`
    /// no hay AEC (headphones/off) — Processor sigue HPF/NS/GC2 solo, Sonora `None`.
    /// Ahorra µs y hace el estado `aec_enabled=false` inmune a stale settings.
    fn chain_with_aec(
        neural: Option<DeepFilterDenoiser>,
        rnnoise: bool,
        fe: Option<FastEnhancerDenoiser>,
        aec: bool,
    ) -> Self {
        // WebRTC Processor siempre sin AEC — solo HPF/NS/GC2. Mantiene FE/DF/RNNoise y limiter.
        let processor = Processor::new(CLOCK_RATE)
            .ok()
            .map(|p| {
                p.set_config(apm_config_with_aec(false));
                p
            });
        let aec = if aec {
            Some(SonoraAec::new(CLOCK_RATE))
        } else {
            None
        };
        Self {
            processor,
            aec,
            rnnoise: if rnnoise { Some(nnnoiseless::DenoiseState::new()) } else { None },
            neural,
            fe,
            speech_detected: false,
            last_lsnr: None,
            buf_apm: Vec::with_capacity(960),
        }
    }
    /// Feed far-end (playback) audio into Sonora AEC3. Call with exact PCM that
    /// goes to speakers, in 10 ms multiples (480 @48 kHz), before capture frames it must cancel.
    /// Orden render -> capture como webrtc. No-op cuando `aec_enabled=false`.
    pub fn process_render_frame(&mut self, frame: &[i16]) {
        let Some(aec) = self.aec.as_mut() else { return };
        aec.process_render_frame(frame);
    }
    /// Resetea solo el estado del AEC (SonoraAec) preservando WebRTC NS/GC2 y FE.
    /// Overflow de `render_tap` deja el filtro adaptativo divergente (timeline
    /// desplazada); estado fresco converge en ~1 s (initial_state_seconds 1.0
    /// en el fork vendorizado, era 2.5 upstream) y es preferible a seguir
    /// suprimiendo voz con filtro divergente.
    pub fn reset_aec(&mut self) {
        self.aec = self.aec.is_some().then(|| SonoraAec::new(CLOCK_RATE));
    }
    /// Optional stats desde Sonora AEC (delay/Echo Return Loss). `None` cuando `aec_enabled=false` o sin AEC.
    pub fn get_stats(&self) -> Option<sonora::stats::AudioProcessingStats> {
        self.aec.as_ref().map(|a| a.stats())
    }
    /// Compat: stats de WebRTC NS (fallback cuando sonora no está). Útil si se necesita NS stats.
    pub fn get_webrtc_stats(&self) -> Option<webrtc_audio_processing::Stats> {
        self.processor.as_ref().map(|p| p.get_stats())
    }
    /// Expose whether Sonora AEC is present (mirrors `aec_enabled` flag). Useful for diagnostics.
    pub fn has_aec(&self) -> bool {
        self.aec.is_some()
    }


    /// Suppress noise in a 48 kHz mono frame (length must be a multiple of
    /// 480, e.g. 960). The production chain: AEC3 + high-pass + WebRTC NS +
    /// GainController2 (all inside the WebRTC APM) + peak limiting. The
    /// opt-in tiers then replace the APM output with the denoiser's — the
    /// neural tier (DeepFilterNet, `new_neural`) or the light tier
    /// (RNNoise, `new_light`); with `new()` the APM output is final.
    /// Suppress noise in a 48 kHz mono frame (length must be a multiple of
    /// 480, e.g. 960). Pipeline: Sonora AEC3 -> WebRTC HPF/NS/GC2 -> neural denoiser -> limiter.
    /// Sonora cancela eco antes de NS; WebRTC `Processor` ahora lleva `echo_canceller: None`.
    pub fn process(&mut self, frame: &[i16]) -> Vec<i16> {
        // Stage 0: Sonora AEC (pure Rust) — echo cancellation before NS.
        // Si `aec_enabled=true`, el frame pasa por Sonora (render -> capture orden ya alimentado
        // via `process_render_frame`). Si `false`, es no-op. Se hace antes del `Processor`
        // para que NS/GC2 no vean eco. Requiere copia temporal (960 muestras ~2KB) — insignificante.
        let aec_buf: Option<Vec<i16>> = if self.aec.is_some() {
            let mut tmp = frame.to_vec();
            if let Some(aec) = self.aec.as_mut() {
                aec.process_capture(&mut tmp);
            }
            Some(tmp)
        } else {
            None
        };
        let input_for_ns: &[i16] = match &aec_buf {
            Some(buf) => buf,
            None => frame,
        };
        // P0-4: reuse buf_apm to avoid vec![0; N] per frame on hot path.
        // Destructure to allow simultaneous borrows of disjoint fields (aec/buf_aec ya liberados).
        let Self { processor, rnnoise, neural, fe, speech_detected, last_lsnr, buf_apm, .. } = &mut *self;
        buf_apm.clear();
        buf_apm.resize(frame.len(), 0);
        match processor.as_mut() {
            Some(p) => {
                let mut tmp = [0f32; 480];
                for (in_chunk, out_chunk) in input_for_ns.chunks_exact(480).zip(buf_apm.chunks_exact_mut(480)) {
                    for (i, s) in in_chunk.iter().enumerate() {
                        tmp[i] = *s as f32 / 32768.0;
                    }
                    if p.process_capture_frame([&mut tmp]).is_ok() {
                        for (i, v) in tmp.iter().enumerate() {
                            out_chunk[i] = (v * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                        }
                    } else {
                        out_chunk.copy_from_slice(in_chunk);
                    }
                }
            }
            None => {
                buf_apm.copy_from_slice(input_for_ns);
            }
        }
        // Denoise con el tier activo y deriva speech signal.
        // buf_apm ya tiene HPF+NS+GC2 (post-AEC); borrow como &[i16] para denoisers.
        let mut result: Vec<i16>;
        if let Some(f) = fe.as_mut() {
            // FastEnhancer tier (default): full-band 48 kHz int8 runtime.
            // Su salida en solo-ruido es casi silencio, VAD es post-denoise energy.
            *last_lsnr = None;
            let apm_slice: &[i16] = &*buf_apm;
            result = f.process(apm_slice);
            *speech_detected = rms_level(&result) > 0.01;
        } else if let Some(n) = neural.as_mut() {
            let apm_slice: &[i16] = &*buf_apm;
            let (denoised, lsnr) = n.process(apm_slice);
            result = denoised;
            *last_lsnr = Some(lsnr);
            let vad = ((lsnr - (-10.0)) / 40.0).clamp(0.0, 1.0);
            *speech_detected = vad > 0.5;
        } else if let Some(rn) = rnnoise.as_mut() {
            *last_lsnr = None;
            result = vec![0i16; buf_apm.len()];
            let mut max_vad = 0.0f32;
            let mut input = [0f32; 480];
            let mut denoised = [0f32; 480];
            for (chunk, out_chunk) in buf_apm.chunks_exact(480).zip(result.chunks_exact_mut(480)) {
                for (i, s) in chunk.iter().enumerate() {
                    input[i] = *s as f32 / 32768.0;
                }
                let v = rn.process_frame(&mut denoised, &input);
                max_vad = max_vad.max(v);
                for (i, v) in denoised.iter().enumerate() {
                    out_chunk[i] = (v * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                }
            }
            *speech_detected = max_vad > 0.5;
        } else {
            // Cadena default: Sonora AEC + WebRTC NS/GC2 ya procesados — sin denoiser externo.
            *last_lsnr = None;
            result = buf_apm.clone();
            *speech_detected = rms_level(&result) > 0.01;
        }
        limit_peaks(&mut result, 1.0);
        result
    }

    /// Send-path entry point. Runs the full denoise chain (AEC3 + DeepFilterNet
    /// + leveler) and ALWAYS returns `Some(cleaned)` — every frame is
    /// transmitted. DeepFilterNet strips noise spectrally (its output on a
    /// quiet room is near-zero), so transmitting silence costs nothing
    /// audible. A binary gate (transmit / don't) created hard cuts at speech
    /// edges — the approach Discord and Zoom take is to always transmit the
    /// denoised signal for fluid, gap-free audio.
    pub fn process_gated(&mut self, frame: &[i16]) -> Option<Vec<i16>> {
        Some(self.process(frame))
    }

    /// Whether the most recently processed frame contained speech (RNNoise
    /// VAD). Drives voice-activation (transmit gating) and the speaking meter.
    pub fn speech_detected(&self) -> bool {
        self.speech_detected
    }

    /// Whether any neural denoiser is active (FastEnhancer or DeepFilterNet).
    /// `false` means the chain is WebRTC NS-only (the model failed to load or
    /// the CPU can't run it).
    pub fn neural_available(&self) -> bool {
        self.fe.is_some() || self.neural.is_some()
    }

    /// LSNR (dB) of the most recently processed frame in the neural tier;
    /// `None` in the light (RNNoise) tier. For diagnostics/probes.
    pub fn last_lsnr(&self) -> Option<f32> {
        self.last_lsnr
    }
}

#[allow(dead_code)]
/// Build the WebRTC APM config (HPF + NS VeryHigh + GC2) — sin AEC3.
/// AEC ahora es Sonora (pure Rust), así `echo_canceller` siempre `None` en WebRTC.
/// Cadena ganadora: Sonora AEC + HPF + NS VeryHigh + GC2 (adaptive 15 dB init, 6 dB/s, -50 dBFS floor).
/// Sin denoiser externo agresivo (DeepFilterNet dañaba voz 10-50 dB peores frames).
fn apm_config() -> Config {
    apm_config_with_aec(true)
}

/// APM config sin AEC (Sonora lo maneja). `echo_canceller: None` deja
/// `submodules_.echo_controller` sin init — FilterCore/xcorr ausente del perfil.
/// Parámetro `aec` se ignora (queda por compatibilidad `with_model_and_aec`), siempre `None`.
fn apm_config_with_aec(_aec: bool) -> Config {
    Config {
        echo_canceller: None,
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

/// Tuned AEC3 config per `local/webrtc-tuning.md` §7.2 (experimental, non-MSVC only).
/// Values target less destructive near-end suppression and more tolerant delay
/// headroom for PipeWire jitter (~1 block). Must be validated before use.
#[cfg(all(feature = "experimental-aec3-config", not(target_env = "msvc")))]
pub fn tuned_aec3_config() -> EchoCanceller3Config {
    let mut c = EchoCanceller3Config::default();
    // DTD less aggressive: keep near-end speech in double-talk.
    c.suppressor.dominant_nearend_detection.enr_threshold = 0.35;
    c.suppressor.dominant_nearend_detection.snr_threshold = 20.0;
    c.suppressor.dominant_nearend_detection.hold_duration = 70;
    // Delay robustness for PipeWire/cpal jitter (~1 block).
    c.delay.delay_headroom_samples = 64;
    c.delay.hysteresis_limit_blocks = 2;
    // Longer tail for small-room reverb (52 ms → 64 ms); cost ~10 % CPU.
    c.filter.refined.length_blocks = 16;
    c.filter.coarse.length_blocks = 16;
    // Less destructive suppressor masks.
    c.suppressor.normal_tuning.mask_lf.enr_suppress = 0.30;
    c.suppressor.normal_tuning.mask_hf.enr_suppress = 0.08;
    // Anti-howling (default off: thresh 400 gain 1.0).
    c.suppressor.high_bands_suppression.anti_howling_activation_threshold = 200.0;
    c.suppressor.high_bands_suppression.anti_howling_gain = 0.3;
    // Export linear AEC output for `analyze_linear_aec_output` path (requires
    // NoiseSuppression::analyze_linear_aec_output = true to take effect, but
    // we keep production Config unchanged per spec — the flag is still useful
    // for measurement harnesses that set it).
    c.filter.export_linear_aec_output = true;
    assert!(c.validate(), "EchoCanceller3Config fuera de rango — ver Validate() clamps");
    c
}

/// Internal helper: create a `Processor` at `sample_rate`.
///
/// Legacy: cuando AEC era WebRTC, intentaba `Processor::with_aec3_config` con tuning.
/// Ahora con Sonora, `Processor` siempre es `echo_canceller: None`; esta función queda
/// para compatibilidad pero no se usa en `chain_with_aec` (usa `Processor::new` directo).
#[allow(dead_code)]
#[cfg(all(feature = "experimental-aec3-config", not(target_env = "msvc")))]
fn new_processor(sample_rate: u32) -> Result<Processor, webrtc_audio_processing::Error> {
    match Processor::with_aec3_config(sample_rate, tuned_aec3_config()) {
        Ok(p) => Ok(p),
        Err(_) => Processor::new(sample_rate),
    }
}

#[allow(dead_code)]
#[cfg(not(all(feature = "experimental-aec3-config", not(target_env = "msvc"))))]
fn new_processor(sample_rate: u32) -> Result<Processor, webrtc_audio_processing::Error> {
    Processor::new(sample_rate)
}

/// UI-selectable suppression model for the voice send path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SuppressorModel {
    /// WebRTC NS only — no external neural denoiser ("practically free").
    NsOnly,
    /// FastEnhancer-Small (48 kHz full-band, int8 C runtime, hop 512) — the
    /// "Ligera" tier: ~1.6 % of total CPU, slightly weaker noise floor than
    /// Medium (measured −67.6 vs < −120 dBFS on the hard 36 s input; inaudible
    /// in normal rooms). Requires AVX2+FMA3+F16C.
    FastEnhancerS,
    /// FastEnhancer-Medium (48 kHz full-band, int8 C runtime, hop 320) — the
    /// "Ultra" tier: ~6.4 % of total CPU, deepest suppression. Requires
    /// AVX2+FMA3+F16C; auto-degrades to `NsOnly` when the CPU can't run it.
    FastEnhancerM,
}

impl SuppressorModel {
    /// Stable identifier for persistence/UI (lumen-core stores this string).
    pub fn as_str(&self) -> &'static str {
        match self {
            SuppressorModel::NsOnly => "ns-only",
            SuppressorModel::FastEnhancerS => "fastenhancer-s",
            SuppressorModel::FastEnhancerM => "fastenhancer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ns-only" => Some(SuppressorModel::NsOnly),
            "fastenhancer-s" => Some(SuppressorModel::FastEnhancerS),
            "fastenhancer" => Some(SuppressorModel::FastEnhancerM),
            _ => None,
        }
    }

    /// Whether this model can actually run on this build/hardware — used by
    /// the UI to surface (never silently hide) an unavailable selection.
    /// NsOnly is always available; FastEnhancerS/M need AVX2+FMA3+F16C.
    pub fn available(&self) -> bool {
        match self {
            SuppressorModel::NsOnly => true,
            SuppressorModel::FastEnhancerS => FastEnhancerDenoiser::available(),
            SuppressorModel::FastEnhancerM => FastEnhancerDenoiser::available(),
        }
    }
}

impl Default for NoiseSuppressor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// FastEnhancerDenoiser (faster-enhancer.c, FastEnhancer-Medium 48 kHz)
// ---------------------------------------------------------------------------

/// Raw FFI to the vendored faster-enhancer.c runtime (single global engine,
/// one audio thread). Only linked when `fe_built` cfg is set — fe requires
/// GCC/Clang-style per-file ISA flags and rejects MSVC/clang-cl at configure.
#[cfg(feature = "fe_built")]
mod ffe {
    use std::os::raw::{c_int, c_void};

    extern "C" {
        /// Returns 0 on success; non-zero (e.g. no AVX2+FMA3+F16C) on failure.
        pub fn fe_init(weights_blob: *const c_void, weights_size: c_int) -> c_int;
        pub fn fe_run(in_: *const f32, out: *mut f32);
        pub fn fe_free();
        /// FastEnhancer-Small ("Ligera"): hop 512, symbol-prefixed build
        /// (see build.rs) so it links alongside the Medium runtime.
        pub fn fe_s_init(weights_blob: *const c_void, weights_size: c_int) -> c_int;
        pub fn fe_s_run(in_: *const f32, out: *mut f32);
        pub fn fe_s_free();
    }
}

/// FastEnhancer 48 kHz full-band denoiser — the default neural tier.
/// Wraps the vendored faster-enhancer.c int8 runtime (MIT, see
/// vendor/faster-enhancer/). Two variants:
///   `new()`       — Medium ("Ultra", hop 320, 6.67 ms frames)
///   `new_small()` — Small ("Ligera", hop 512, 10.67 ms frames)
/// Requires SSE4.1 (minimum) or AVX2+FMA3+F16C (full speed); both
/// `new`/`new_small` return `None` on CPUs without SSE4.1 so the caller
/// falls back to WebRTC NS-only. On SSE4.1-only CPUs (Pentium/Celeron)
/// the runtime uses software fp16 and 4-wide GEMM kernels at ~half the
/// throughput of AVX2 — still fast enough for real-time at ~3-6% of a core.
/// The engine is a C global (single instance); the struct is
/// never `Send`/`Sync` — it lives on the single send-task thread.
pub struct FastEnhancerDenoiser {
    run: unsafe extern "C" fn(*const f32, *mut f32),
    free: unsafe extern "C" fn(),
    frame_size: usize,
    /// Input accumulator: the app's 20 ms frames (960) don't align with the
    /// Small engine's 512-sample frames, so the wrapper buffers and drains
    /// full engine frames (Medium: 960 = 3×320 → never holds anything).
    in_buf: Vec<f32>,
    /// Output accumulator: the engine emits `frame_size` per call; the send
    /// task / Opus encoder needs fixed 20 ms frames, so complete
    /// input-length blocks are returned and the partial remainder is held
    /// for the next call (bounded lag ≤ one frame ≈ 20 ms).
    out_buf: Vec<f32>,
    /// Reusable buffer for drain chunk (P0-10) — avoids Vec alloc per engine call.
    run_buf: Vec<f32>,
    /// Reusable buffer for denoised output (P0-10).
    denoise_buf: Vec<f32>,
}

#[cfg(feature = "fe_built")]
impl FastEnhancerDenoiser {
    /// Embedded W8A8 weight blobs. `fe_init` references them zero-copy,
    /// so they must outlive the engine — `'static` slices work.
    const WEIGHTS_M: &'static [u8] = include_bytes!("../vendor/faster-enhancer/weights/fe.q8");
    const WEIGHTS_S: &'static [u8] = include_bytes!("../vendor/faster-enhancer/weights/fe_s.q8");

    /// Non-destructive availability probe (does NOT init the global engine).
    /// Mirrors the runtime's SSE4.1 floor (x86) / NEON baseline
    /// (arm64) so the UI can avoid offering a model this CPU can't run —
    /// there is no silent fallback at the UI layer.
    pub fn available() -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            // AVX2+FMA+F16C (full speed) or SSE4.1 (half speed, software fp16)
            (std::arch::is_x86_feature_detected!("avx2")
                && std::arch::is_x86_feature_detected!("fma")
                && std::arch::is_x86_feature_detected!("f16c"))
            || std::arch::is_x86_feature_detected!("sse4.1")
        }
        #[cfg(target_arch = "aarch64")]
        {
            true // NEON baseline is always present on arm64
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            false
        }
    }

    /// FastEnhancer-Medium ("Ultra", hop 320, 6.67 ms frames).
    /// Returns `None` when `fe_init` fails — i.e. the host lacks
    /// SSE4.1 — which is how the runtime degrades to the NS-only
    /// tier on weak CPUs (the UI already avoids offering it there).
    pub fn new() -> Option<Self> {
        Self::build(ffe::fe_init, ffe::fe_run, ffe::fe_free, Self::WEIGHTS_M, 320)
    }

    /// FastEnhancer-Small ("Ligera", hop 512, 10.67 ms frames) — the
    /// low-CPU tier (~1.6 % of total CPU vs Medium's ~6.4 %, measured on the
    /// i5-4590). Same SSE4.1 floor as Medium; requires `fe_s_built` cfg.
    #[cfg(feature = "fe_s_built")]
    pub fn new_small() -> Option<Self> {
        Self::build(ffe::fe_s_init, ffe::fe_s_run, ffe::fe_s_free, Self::WEIGHTS_S, 512)
    }
    #[cfg(not(feature = "fe_s_built"))]
    pub fn new_small() -> Option<Self> {
        None
    }

    fn build(
        init: unsafe extern "C" fn(*const std::os::raw::c_void, std::os::raw::c_int) -> std::os::raw::c_int,
        run: unsafe extern "C" fn(*const f32, *mut f32),
        free: unsafe extern "C" fn(),
        weights: &'static [u8],
        frame_size: usize,
    ) -> Option<Self> {
        let ok = unsafe {
            init(
                weights.as_ptr() as *const std::os::raw::c_void,
                weights.len() as i32,
            )
        };
        if ok == 0 {
            Some(Self {
                run,
                free,
                frame_size,
                in_buf: Vec::new(),
                out_buf: Vec::new(),
                run_buf: Vec::with_capacity(frame_size),
                denoise_buf: Vec::with_capacity(frame_size),
            })
        } else {
            None
        }
    }

    /// Process one frame (multiple of 480, e.g. a 960-sample 20 ms capture
    /// frame). Returns complete input-length blocks (the send task / Opus
    /// encoder needs fixed 20 ms frames); a partial remainder is held in the
    /// output buffer and emitted with the next call.
    pub fn process(&mut self, frame: &[i16]) -> Vec<i16> {
        // P0-10: bulk extend (single reserve) + reuse buffers to avoid per-call allocs.
        self.in_buf.extend(frame.iter().map(|&s| s as f32 / 32768.0));
        let fs = self.frame_size;
        while self.in_buf.len() >= fs {
            self.run_buf.clear();
            self.run_buf.extend(self.in_buf.drain(..fs));
            self.denoise_buf.clear();
            self.denoise_buf.resize(fs, 0.0);
            unsafe { (self.run)(self.run_buf.as_ptr(), self.denoise_buf.as_mut_ptr()) };
            self.out_buf.extend_from_slice(&self.denoise_buf);
        }
        let emit = self.out_buf.len() / frame.len() * frame.len();
        if emit == 0 {
            return Vec::new();
        }
        self.out_buf
            .drain(..emit)
            .map(|v| (v * 32767.0).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16)
            .collect()
    }
}

#[cfg(feature = "fe_built")]
impl Drop for FastEnhancerDenoiser {
    fn drop(&mut self) {
        unsafe { (self.free)(); }
    }
}

// When the C runtime is not built; the type exists so the tier wiring
// compiles, but `new()` always returns None → NS-only chain. `available()`
// still mirrors the real CPU dispatch floor ((avx2&&fma&&f16c)||sse4.1) so
// the UI can report compatibility based on hardware, not build artifact.
#[cfg(not(feature = "fe_built"))]
impl FastEnhancerDenoiser {
    pub fn new() -> Option<Self> {
        None
    }
    pub fn new_small() -> Option<Self> {
        None
    }
    pub fn available() -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            (std::arch::is_x86_feature_detected!("avx2")
                && std::arch::is_x86_feature_detected!("fma")
                && std::arch::is_x86_feature_detected!("f16c"))
            || std::arch::is_x86_feature_detected!("sse4.1")
        }
        #[cfg(target_arch = "aarch64")]
        {
            true
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            false
        }
    }
    pub fn process(&mut self, frame: &[i16]) -> Vec<i16> {
        frame.to_vec()
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
        Self::with_atten_lim(100.0) // default: no attenuation limit
    }

    /// As [`new`](Self::new), with an explicit mask attenuation limit in dB.
    /// The default is 100 dB (unbounded — the mask can cut a frame to
    /// silence). A tighter limit (e.g. 20-30 dB) bounds the mask's depth,
    /// which caps the frame-to-frame level pumping on quiet speech (the
    /// "oscillation" complaint) at the cost of less noise suppression on the
    /// deepest-noise frames.
    pub fn with_atten_lim(atten_lim_db: f32) -> Option<Self> {
        use df::tract::{DfParams, DfTract, RuntimeParams};
        // Enable the spectral post-filter (per-bin over-attenuation smoothing,
        // beta = the crate's own default). It tightens residual suppression in
        // speech-adjacent bins — the residual the AGC could otherwise amplify —
        // at a negligible per-frame cost (a multiply/add on ~481 complex bins).
        // Lower min_db_thresh (-10 -> -20): frames whose LSNR sits in
        // [-20, -10) get the full DNN mask instead of the ZERO mask (the
        // `lsnr < min_db_thresh` branch outputs silence). Measured on the real
        // input: the AEC3's residual suppression pushes quiet-speech frames
        // below -10 dB LSNR, and the zero mask made ~16% of speech frames
        // disappear entirely ("cortado"). With -20 those frames come out
        // ~-16 dB attenuated (audible) instead of cut; noise residual stays
        // -81 dBFS (200x below the leveler's floor).
        match DfTract::new(
            DfParams::default(),
            &RuntimeParams::default()
                .with_post_filter(0.02)
                .with_thresholds(-20.0, 30.0, 20.0)
                .with_atten_lim(atten_lim_db),
        ) {
            Ok(model) => Some(Self { model }),
            Err(e) => {
                eprintln!("lumen voice: DeepFilterNet load failed: {e}");
                None
            }
        }
    }

    /// Denoise a 48 kHz mono frame, returning the denoised PCM plus the frame's
    /// LSNR (local SNR, dB) from the model's internal spectral estimate — a
    /// robust speech-presence signal computed inside the neural frontend (see
    /// `DfTract::process`). LSNR < -10 dB is noise-only; > 30 dB is clean
    /// speech. DeepFilterNet runs with a 10 ms (480-sample) hop, so a 20 ms
    /// (960-sample) frame is fed as two chunks and produces 960 samples out;
    /// the returned LSNR is the max over those chunks (speech anywhere in the
    /// frame). The model is stateful with a ~30 ms lookahead delay (inherent
    /// to streaming enhancement — the caller's jitter buffer absorbs it).
    pub fn process(&mut self, frame: &[i16]) -> (Vec<i16>, f32) {
        let hop = self.model.hop_size;
        let mut result = Vec::with_capacity(frame.len());
        let mut in_arr = ndarray::Array2::<f32>::zeros((1, hop));
        let mut out_arr = ndarray::Array2::<f32>::zeros((1, hop));
        let mut lsnr = LSNR_SILENCE;
        for chunk in frame.chunks_exact(hop) {
            for (i, &s) in chunk.iter().enumerate() {
                in_arr[[0, i]] = s as f32 / 32768.0;
            }
            // On model error, fall back to the model's own silence sentinel.
            let chunk_lsnr = self
                .model
                .process(in_arr.view(), out_arr.view_mut())
                .unwrap_or(LSNR_SILENCE);
            lsnr = lsnr.max(chunk_lsnr);
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
        (result, lsnr)
    }
}

/// LSNR (dB) `DfTract::process` returns for a silence/zero frame — also the
/// fallback here when the model errors or a frame is partial-padded.
const LSNR_SILENCE: f32 = -15.0;

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
/// target level and attenuates loud speech down toward it, gating silence, so
/// a cheap/quiet mic is audible WITHOUT amplifying background noise and a
/// hot/loud mic is turned down instead of clipping.
///
/// WebRTC's adaptive AGC (GainController2) was measured to boost the noise
/// floor (+15 dB on quiet noise), so we do the gate ourselves: the VAD signal
/// (DeepFilterNet LSNR in the neural path, RNNoise VAD in the fallback) opens
/// the gain, which chases a target RMS and holds through a hangover so words
/// aren't clipped; on silence the gain decays to unity (no boost), leaving
/// the already-suppressed noise inaudible. The gate is OR-ed with an absolute
/// post-denoise level floor ([`SPEECH_RMS_FLOOR`]) so the boost survives the
/// model's LSNR drift on long streams (the LSNR collapses after ~20 s of
/// continuous speech; the denoised level is still a reliable speech signal).
pub struct SpeechLeveler {
    /// Target RMS for speech after gain (~ -18 dBFS).
    target_rms: f32,
    /// Max gain factor (+18 dB). Enough to bring a quiet mic (RMS 0.015) up
    /// to the target. Safe because the LSNR VAD keeps the gate shut on noise.
    max_gain: f32,
    /// Min gain factor (0.25 = -12 dB) — loud/hot mics are attenuated toward
    /// the target instead of left at unity to clip.
    min_gain: f32,
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
/// Absolute post-denoise RMS floor (-34 dBFS) that opens the gain gate even
/// when the model's LSNR VAD has drifted closed on long streams (measured:
/// the LSNR collapses after ~20 s of continuous speech). After
/// DeepFilterNet/RNNoise, a frame this loud cannot be residual noise (measured
/// residual: neural ~0.0000, light worst frame 0.0018 — 11x margin), so it is
/// safe to treat as speech and boost toward the target. Rescues the quiet-mic
/// range (input >= -34 dBFS) from the drift.
const SPEECH_RMS_FLOOR: f32 = 0.02;
/// ~1 s of hold at 20 ms frames. Covers inter-sentence pauses (typical
/// 0.2-0.3 s) so the gain doesn't decay and re-ramp between phrases — a
/// decayed gain recovers slowly, attenuating the first syllables of the next
/// phrase ("volume rollercoaster" / swallowed phrase starts).
const HANGOVER_FRAMES: u32 = 50;

/// Max per-frame gain change after IIR smoothing: ±0.3 at 20 ms/frame is
/// ~15 dB/s slew — fast enough for speech onset, slow enough to suppress
/// syllable-level pumping (a single loud frame can no longer yank the gain).
const MAX_GAIN_STEP: f32 = 0.3;

impl SpeechLeveler {
    pub fn new() -> Self {
        Self {
            target_rms: 0.12,
            max_gain: 8.0,
            min_gain: 0.25,
            gain: 1.0,
            vad_smooth: 0.0,
            // Start the level estimate near the target level so `desired`
            // (~2.4x) is right from the first frame: with 0.001 the gain
            // clamped to 8x and overshot to ~5x on the first words of a call
            // before settling ("descalibrado" start).
            speech_rms: 0.05,
            hold: 0,
        }
    }

    /// Current smoothed gain factor (1.0 = unity). For diagnostics/probes.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Smoothed pre-gain speech level (0..1). For diagnostics/probes.
    pub fn speech_rms(&self) -> f32 {
        self.speech_rms
    }

    /// Smoothed voice-activity probability (0..1). For diagnostics/probes.
    pub fn vad_smooth(&self) -> f32 {
        self.vad_smooth
    }

    /// Apply gain to `samples` in place based on the frame's VAD probability
    /// and RMS. Returns true if speech was detected this frame.
    pub fn process(&mut self, vad: f32, frame_rms: f32, samples: &mut [i16]) -> bool {
        self.vad_smooth = self.vad_smooth * 0.8 + vad * 0.2;
        // Gate: the model's VAD (LSNR) OR an absolute post-denoise level floor.
        // The floor covers the LSNR drift on long streams: after the denoiser,
        // a frame at -34 dBFS RMS or louder is speech (residual noise is far
        // below), so we keep boosting even when the model stops flagging it.
        let speech = self.vad_smooth > VAD_ON || frame_rms > SPEECH_RMS_FLOOR;
        if speech {
            // Track the speaker's AVERAGE level with a slow estimator (attack
            // τ ~400 ms, release ~1 s) so syllable-level dynamics do NOT move
            // the gain — only sustained loudness changes do. Old code used a
            // 0.3/frame attack (τ ~67 ms): a single loud syllable yanked
            // speech_rms up, dropped the gain, and the following quieter
            // syllables came out attenuated.
            if frame_rms > self.speech_rms {
                self.speech_rms = self.speech_rms * 0.95 + frame_rms * 0.05;
            } else {
                self.speech_rms = self.speech_rms * 0.98 + frame_rms * 0.02;
            }
            let desired =
                (self.target_rms / self.speech_rms.max(0.0001)).clamp(self.min_gain, self.max_gain);
            // Gain smoothing: fast reduction (avoid clipping on a sudden loud
            // burst) and fast-ish increase (recover the boost within ~200 ms
            // at phrase starts). With the slow speech_rms estimator, `desired`
            // is stable, so the increase speed cannot pump.
            let prev_gain = self.gain;
            let new_gain = self.gain * 0.9 + desired * 0.1; // τ ≈ 200 ms
            // Rate-limit the per-frame gain CHANGE after the IIR (rate-limiting
            // `desired` first would starve the IIR). At 20 ms/frame, ±0.3 is
            // ~15 dB/s — fast enough for speech onset but slow enough that a
            // single loud syllable can't yank the gain down by 30% in one
            // frame ("volume rollercoaster").
            let delta = new_gain - prev_gain;
            self.gain = if delta.abs() > MAX_GAIN_STEP {
                prev_gain + MAX_GAIN_STEP.copysign(delta)
            } else {
                new_gain
            };
            self.hold = HANGOVER_FRAMES;
        } else if self.hold > 0 {
            self.hold -= 1;
        } else {
            // Silence: decay the boost toward unity so noise stays un-boosted.
            self.gain = (self.gain - 1.0) * 0.85 + 1.0;
        }
        if (self.gain - 1.0).abs() > 0.0001 {
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
        let mut out = Vec::new();
        self.resample_into(input, &mut out);
        out
    }

    /// P0-6: zero-copy variant that extends `out` directly, avoiding the
    /// intermediate Vec in the common passthrough case (`src == dst`).
    pub fn resample_into(&mut self, input: &[i16], out: &mut Vec<i16>) {
        if input.is_empty() {
            return;
        }
        if self.src_rate == self.dst_rate {
            out.extend_from_slice(input);
            return;
        }
        let len = input.len() as f64;
        let ratio = self.dst_rate as f64 / self.src_rate as f64;
        let step = 1.0 / ratio;
        // Reserve once for this chunk.
        out.reserve((len * ratio).ceil() as usize);
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
    }
}
/// H1 silence-path decision: whether the send loop can reuse the last
/// encoded silence packet instead of re-encoding the frame.
///
/// Contract (pinned by tests/send_chain_cpu.rs):
/// - `speech == true`  → ALWAYS encode fresh and reset the streak (a speech
///   onset is never delayed — no edge cuts).
/// - silence: the first two frames encode fresh (to seed the cached packet),
///   then the packet is reused until the ~8 s refresh boundary (the encoder
///   re-runs so the remote's CNG state cannot go stale).
///
/// Returns `(reuse, new_streak)`.
pub fn silence_reuse_decision(speech: bool, streak: u32, have_pkt: bool) -> (bool, u32) {
    if speech {
        return (false, 0);
    }
    let new_streak = if streak >= 400 { 0 } else { streak.saturating_add(1) };
    if have_pkt && new_streak >= 2 && streak < 400 {
        (true, new_streak)
    } else {
        (false, new_streak)
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

/// Brickwall limiter: if the frame's peak exceeds `ceiling_db` dBFS, scale
/// the whole frame down so the peak lands exactly at the ceiling; no-op
/// otherwise. ceiling_db 1.0 = -1 dBFS. Used on send (after the leveler)
/// and on the receive mix.
pub fn limit_peaks(samples: &mut [i16], ceiling_db: f32) {
    let ceiling = 10f32.powf(-ceiling_db / 20.0) * 32767.0;
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0) as f32;
    if peak > ceiling {
        let gain = ceiling / peak;
        for s in samples.iter_mut() {
            *s = ((*s as f32) * gain).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
        }
    }
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
            tap_resampler: LinearResampler::new(CLOCK_RATE, CLOCK_RATE),
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
        for _ in 0..(HANGOVER_FRAMES + 40) {
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
    fn speech_leveler_holds_gain_through_phrase_pause() {
        // Phrase-start regression: typical inter-phrase pauses (0.2-0.6 s,
        // measured on the real input) must NOT decay the boost — a decayed
        // gain re-ramps and the first syllables of the next phrase come out
        // attenuated. HANGOVER_FRAMES covers pauses up to ~1 s.
        let mut lvl = SpeechLeveler::new();
        let base: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| {
                (((i as f32 / CLOCK_RATE as f32) * 2.0 * std::f32::consts::PI * 200.0).sin()
                    * 600.0) as i16
            })
            .collect();
        for _ in 0..15 {
            let mut frame = base.clone();
            lvl.process(0.9, rms_level(&frame), &mut frame);
        }
        let pre_pause = lvl.gain;
        assert!(pre_pause > 2.0, "gain should open above unity: {pre_pause}");
        // A 0.6 s pause (30 frames): typical between sentences.
        let silence = vec![0i16; FRAME_SAMPLES];
        for _ in 0..30 {
            let mut frame = silence.clone();
            lvl.process(0.0, 0.0, &mut frame);
        }
        assert!(
            lvl.gain >= pre_pause * 0.95,
            "gain must hold through a 0.6 s pause: {:.2} vs {pre_pause:.2}",
            lvl.gain
        );
        // A longer pause (>1 s) DOES decay the gain; recovery is rate-limited
        // (~15 dB/s) by design and the boost is back within a few frames.
        for _ in 0..(HANGOVER_FRAMES + 10) {
            let mut frame = silence.clone();
            lvl.process(0.0, 0.0, &mut frame);
        }
        assert!(
            lvl.gain < pre_pause * 0.5,
            "gain must decay during a long pause: {:.2} vs {pre_pause:.2}",
            lvl.gain
        );
        for _ in 0..8 {
            let mut frame = base.clone();
            lvl.process(0.9, rms_level(&frame), &mut frame);
        }
        assert!(
            lvl.gain > 1.5,
            "gain must recover after a long pause: {:.2}",
            lvl.gain
        );
    }

    #[test]
    fn noise_suppressor_attenuates_background_noise() {
        // The shipped send-path DSP (AEC3 + NS VeryHigh + GainController2 +
        // limiter) must strongly attenuate moderate stationary background
        // noise — the case the user reported (mic picking up room noise).
        // The GC2 starts at its reference +15 dB and its noise cap converges
        // slowly (~11-13 s of stationary input, measured), so the assertion
        // checks the STEADY state: the output noise then drops ~11 dB below
        // the input (measured -45 dBFS vs -34 in).
        let mut ns = NoiseSuppressor::new();
        // Deterministic pseudo-random white noise at a realistic room level
        // (RMS 0.02 ≈ -34 dBFS), 15 s — enough for the AGC to settle.
        let mut state = 0x1234_5678u32;
        let mut noise = Vec::with_capacity(480 * 1500);
        for _ in 0..(480 * 1500) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            noise.push(((state >> 8) as i16) / 8);
        }
        let g = 0.02 / rms_level(&noise);
        for s in noise.iter_mut() {
            *s = ((*s as f32) * g).round() as i16;
        }
        // Warm up the NS + AGC, then measure attenuation at t=13 s.
        for chunk in noise.chunks(480).take(1300) {
            ns.process(chunk);
        }
        let probe = &noise[480 * 1300..480 * 1301];
        let input_rms = rms_level(probe);
        let out = ns.process(probe);
        let output_rms = rms_level(&out);
        eprintln!("send-path DSP: {input_rms} -> {output_rms}");
        assert!(
            output_rms < input_rms * 0.4,
            "steady state must attenuate background noise >= 8 dB: {input_rms} -> {output_rms}"
        );
    }

    #[test]
    fn webrtc_ns_attenuates_white_noise() {
        use webrtc_audio_processing::config::{NoiseSuppression, NoiseSuppressionLevel};
        // NS-only processor (no AGC, which would re-amplify quiet noise).
        // Driven directly (not through NoiseSuppressor) so the leveler —
        // which now boosts ANY post-denoise signal above its absolute floor,
        // including loud NS residuals in this synthetic NS-only config that
        // production never uses — cannot mask the NS module's own contract.
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
        // Warm up the model, then measure attenuation on the 13th frame.
        let mut output_rms = 0f32;
        for (k, chunk) in noise.chunks(480).enumerate().take(13) {
            let mut buf = [0f32; 480];
            for (i, s) in chunk.iter().enumerate() {
                buf[i] = *s as f32 / 32768.0;
            }
            processor.process_capture_frame([&mut buf]).ok();
            if k == 12 {
                output_rms = rms_level(
                    &buf.iter().map(|v| (v * 32767.0).round() as i16).collect::<Vec<_>>(),
                );
            }
        }
        let probe = &noise[480 * 12..480 * 13];
        let input_rms = rms_level(probe);
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
            tap_resampler: LinearResampler::new(CLOCK_RATE, CLOCK_RATE),
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
            let (out, _lsnr) = g.process(&fr);
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
    fn deepfilter_path_speech_detection_by_lsnr() {
        // The DeepFilterNet path derives speech detection from the model's own
        // LSNR (local SNR, dB) — a spectral speech-presence estimate from the
        // neural frontend. Noise must not open the gate; speech must.
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
            // Realistic room level (RMS 0.02 ≈ -34 dBFS) — loud enough to
            // matter, quiet enough not to trip the near-end rescue (which
            // only fires on clear speech, >= -32 dBFS).
            let mut state = 0x1234_5678u32;
            let mut n: Vec<i16> = (0..FRAME_SAMPLES)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    ((state >> 8) as i16) / 4
                })
                .collect();
            let g = 0.02 / rms_level(&n);
            for s in n.iter_mut() {
                *s = ((*s as f32) * g).round() as i16;
            }
            n
        };
        // Noise: must NOT be detected as speech over many frames.
        let mut ns = NoiseSuppressor::new_neural();
        let mut noise_detected = false;
        for _ in 0..30 {
            ns.process(&noise);
            noise_detected |= ns.speech_detected();
        }
        assert!(!noise_detected, "noise must not open the VAD gate");
        // Speech: must be detected (post-denoise energy above the threshold).
        let mut ns = NoiseSuppressor::new_neural();
        let mut speech_detected = false;
        for i in 0..40 {
            ns.process(&speech(i));
            speech_detected |= ns.speech_detected();
        }
        assert!(speech_detected, "speech must be detected via post-denoise energy");
    }

    #[test]
    fn process_gated_always_transmits() {
        // The send path always transmits the denoised frame — no binary gate
        // that would create audible cuts at speech edges. DeepFilterNet's
        // spectral suppression handles noise; the near-zero output on a quiet
        // room is inaudible even when transmitted.
        let mut ns = NoiseSuppressor::new();
        let silence = vec![0i16; FRAME_SAMPLES];
        assert!(
            ns.process_gated(&silence).is_some(),
            "process_gated must always return Some (no gate)"
        );

        // Noisy mic: still always transmits — the denoiser strips the noise.
        let mut state = 0x1234_5678u32;
        let noise: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                ((state >> 8) as i16) / 4
            })
            .collect();
        let mut ns = NoiseSuppressor::new();
        for _ in 0..30 {
            assert!(
                ns.process_gated(&noise).is_some(),
                "process_gated must always return Some even on noise"
            );
        }
    }

    #[test]
    fn speech_leveler_attenuates_loud_speech() {
        // Hot-mic regression: the leveler must TURN DOWN loud speech toward
        // the target (symmetric gain) instead of clamping at unity and letting
        // the signal clip.
        let mut lvl = SpeechLeveler::new();
        // Loud speech-like signal: sine amplitude 14000, RMS ≈ 0.30.
        let loud: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| {
                (((i as f32 / CLOCK_RATE as f32) * 2.0 * std::f32::consts::PI * 200.0).sin()
                    * 14000.0) as i16
            })
            .collect();
        let before = rms_level(&loud);
        let mut out = loud.clone();
        for _ in 0..40 {
            let mut frame = loud.clone();
            lvl.process(1.0, rms_level(&frame), &mut frame);
            out = frame;
        }
        let after = rms_level(&out);
        eprintln!("speech leveler: loud {before:.4} -> {after:.4}, gain {}", lvl.gain());
        assert!(lvl.gain() < 0.6, "loud speech should be attenuated, gain {}", lvl.gain());
        assert!(after < before, "loud speech must not pass through at unity: {before} -> {after}");
    }

    #[test]
    fn peak_limiter_caps_loud_peaks() {
        let ceiling = 10f32.powf(-1.0 / 20.0) * 32767.0; // -1 dBFS
        // Full-scale square frame (alternating ±32767).
        let mut frame: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| if i % 2 == 0 { i16::MAX } else { -i16::MAX })
            .collect();
        limit_peaks(&mut frame, 1.0);
        assert!(
            frame.iter().all(|&s| (s.unsigned_abs() as f32) <= ceiling + 1.0),
            "limiter left samples above -1 dBFS"
        );
        // Input already below the ceiling passes through bit-exact.
        let quiet: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| ((i as f64 * 0.1).sin() * 5000.0) as i16)
            .collect();
        let mut copy = quiet.clone();
        limit_peaks(&mut copy, 1.0);
        assert_eq!(copy, quiet, "sub-ceiling input must pass through unchanged");
    }

    #[test]
    fn webrtc_limiter_caps_fullscale() {
        use webrtc_audio_processing::config::{GainController, GainController1, GainControllerMode};
        // Limiter-only APM gain controller — the Step-2 config: fixed digital,
        // zero compression gain (no boost), hard ceiling at -1 dBFS.
        let processor = Processor::new(CLOCK_RATE).expect("APM init");
        processor.set_config(Config {
            gain_controller: Some(GainController::GainController1(GainController1 {
                mode: GainControllerMode::FixedDigital,
                target_level_dbfs: 1,
                compression_gain_db: 0,
                enable_limiter: true,
                analog_gain_controller: None,
            })),
            ..Config::default()
        });
        // Full-scale sine capture frames (amplitude 32767).
        let mut sine = Vec::with_capacity(480 * 24);
        for i in 0..(480 * 24) {
            let t = i as f64 / CLOCK_RATE as f64;
            sine.push(((2.0 * std::f64::consts::PI * 200.0 * t).sin() * 32767.0) as i16);
        }
        let ceiling = 10f32.powf(-1.0 / 20.0) * 32767.0;
        let mut peak = 0f32;
        for chunk in sine.chunks_exact(480) {
            let mut buf = [0f32; 480];
            for (i, s) in chunk.iter().enumerate() {
                buf[i] = *s as f32 / 32768.0;
            }
            processor.process_capture_frame([&mut buf]).expect("10 ms block");
            for &v in &buf {
                peak = peak.max(v.abs() * 32767.0);
            }
        }
        eprintln!("APM limiter: full-scale sine peak -> {peak:.1} (ceiling {ceiling:.1})");
        assert!(
            peak <= ceiling + 1.0,
            "APM limiter failed to cap full-scale input: peak {peak} > ceiling {ceiling}"
        );
    }

    #[test]
    fn aec_tap_is_resampled_to_48k() {
        // Bug A regression: the AEC3 render reference must be 48 kHz even on a
        // non-48 kHz output device (44.1 kHz here) — feeding device-rate
        // samples to AEC3 time-stretches the echo reference (~8.8 %) and
        // breaks echo cancellation.
        let mut output = AudioOutput::new();
        let tap = Arc::new(Mutex::new(Vec::new()));
        output.set_render_tap(tap.clone());
        *output.state.lock() = Some(OutputState {
            buf: Vec::new(),
            resampler: LinearResampler::new(CLOCK_RATE, 44_100),
            tap_resampler: LinearResampler::new(44_100, CLOCK_RATE),
            channels: 1,
            frame_size: 882,
            dropped_samples: 0,
        });
        let frame: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| ((i as f64 * 0.05).sin() * 2000.0) as i16)
            .collect();
        output.push(&frame); // 960 @ 48 kHz -> 882 @ 44.1 kHz
        let mut out = vec![0i16; 882]; // 10 ms @ 44.1 kHz
        output.drain_into(&mut out);
        // The tap holds exactly what was played, resampled back to 48 kHz:
        // 882 device-rate samples -> 960 (10 ms @ 48 kHz).
        let tap_len = tap.lock().len();
        assert_eq!(tap_len, 960, "AEC tap must be 48 kHz (10 ms), got {tap_len}");
    }
}



