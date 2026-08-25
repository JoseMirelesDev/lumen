//! Vox UI sound layer — minimalist, non-gamey.
//!
//! Plays the synthesized UI kit (`sounds/*.wav`, 48 kHz mono PCM-i16) through
//! cpal on a dedicated output stream. Design rules (see
//! `scripts/gen-ui-sounds.py`): short one-shots (< 220 ms), quiet (-16 to
//! -26 dBFS), no melodic loops. Overlapping plays mix additively and clamp.
//!
//! Failure is non-fatal: if no output device exists (headless CI, broken
//! ALSA) the layer silently disables itself and every `play` is a no-op.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;

pub const SOUND_JOIN: &[u8] = include_bytes!("../sounds/join.wav");
pub const SOUND_LEAVE: &[u8] = include_bytes!("../sounds/leave.wav");
pub const SOUND_MUTE_ON: &[u8] = include_bytes!("../sounds/mute-on.wav");
pub const SOUND_MUTE_OFF: &[u8] = include_bytes!("../sounds/mute-off.wav");
pub const SOUND_DEAFEN_ON: &[u8] = include_bytes!("../sounds/deafen-on.wav");
pub const SOUND_DEAFEN_OFF: &[u8] = include_bytes!("../sounds/deafen-off.wav");
pub const SOUND_SEND: &[u8] = include_bytes!("../sounds/send.wav");
pub const SOUND_RECEIVE: &[u8] = include_bytes!("../sounds/receive.wav");
pub const SOUND_USER_JOIN: &[u8] = include_bytes!("../sounds/user-join.wav");
pub const SOUND_USER_LEFT: &[u8] = include_bytes!("../sounds/user-left.wav");
pub const SOUND_ERROR: &[u8] = include_bytes!("../sounds/error.wav");
pub const SOUND_CALL: &[u8] = include_bytes!("../sounds/call.wav");

/// One short sound. Decoded + resampled to the output device rate at startup.
pub struct Sound {
    samples: Arc<Vec<i16>>,
}

impl Sound {
    fn decode(wav: &'static [u8], out_rate: u32) -> Sound {
        let Some(pcm) = parse_wav_pcm_i16(wav) else {
            return Sound::default();
        };
        let samples = if out_rate == 48_000 {
            pcm
        } else {
            resample_lerp(&pcm, 48_000, out_rate)
        };
        Sound {
            samples: Arc::new(samples),
        }
    }
}

impl Default for Sound {
    fn default() -> Self {
        Self {
            samples: Arc::new(Vec::new()),
        }
    }
}

/// Minimal RIFF parser for the exact files this kit ships (48 kHz mono i16).
fn parse_wav_pcm_i16(wav: &[u8]) -> Option<Vec<i16>> {
    if wav.len() < 44 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12usize;
    while pos + 8 <= wav.len() {
        let id = &wav[pos..pos + 4];
        let size = u32::from_le_bytes(wav[pos + 4..pos + 8].try_into().ok()?) as usize;
        let body = pos + 8;
        if id == b"fmt " {
            let channels = u16::from_le_bytes(wav.get(body + 2..body + 4)?.try_into().ok()?);
            let rate = u32::from_le_bytes(wav.get(body + 4..body + 8)?.try_into().ok()?);
            let bits = u16::from_le_bytes(wav.get(body + 14..body + 16)?.try_into().ok()?);
            if channels != 1 || rate != 48_000 || bits != 16 {
                return None;
            }
        } else if id == b"data" {
            let n = size.min(wav.len() - body);
            let bytes = &wav[body..body + n];
            let mut out = Vec::with_capacity(n / 2);
            for chunk in bytes.chunks_exact(2) {
                out.push(i16::from_le_bytes([chunk[0], chunk[1]]));
            }
            return Some(out);
        }
        pos = body + size + (size & 1); // chunks are word-aligned
    }
    None
}

/// Cheap linear resampler for short UI blips (no aliasing concern at these
/// frequencies; the kit is one-shot gestures, not music).
fn resample_lerp(src: &[i16], src_rate: u32, dst_rate: u32) -> Vec<i16> {
    let ratio = src_rate as f64 / dst_rate as f64;
    let n = ((src.len() as f64) / ratio) as usize;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let pos = i as f64 * ratio;
        let i0 = pos.floor() as usize;
        let i1 = (i0 + 1).min(src.len() - 1);
        let frac = (pos - i0 as f64) as f32;
        out.push((src[i0] as f32 * (1.0 - frac) + src[i1] as f32 * frac) as i16);
    }
    out
}

/// An in-flight one-shot.
struct Voice {
    samples: Arc<Vec<i16>>,
    pos: usize,
    gain: f32,
}

/// Additive mixer shared with the cpal callback. Mono voices fan out to all
/// output channels.
struct Mixer {
    voices: Vec<Voice>,
    kept: Vec<Voice>,
}
impl Mixer {
    fn mix(&mut self, channels: usize, out: &mut [f32]) {
        for o in out.iter_mut() {
            *o = 0.0;
        }
        if self.voices.is_empty() {
            return;
        }
        let frames = out.len() / channels;
        self.kept.clear();
        for mut v in self.voices.drain(..) {
            let take = frames.min(v.samples.len() - v.pos);
            for f in 0..take {
                let s = v.samples[v.pos + f] as f32 / 32768.0 * v.gain;
                for c in 0..channels {
                    out[f * channels + c] += s;
                }
            }
            v.pos += take;
            if v.pos < v.samples.len() {
                self.kept.push(v);
            }
        }
        std::mem::swap(&mut self.voices, &mut self.kept);
    }
}

/// Playback handle. Cheap to clone; all methods are no-ops when audio failed.
#[derive(Clone)]
pub struct Sfx {
    sounds: Arc<SfxTable>,
    mixer: Arc<Mutex<Mixer>>,
    alive: Arc<AtomicBool>,
}

struct SfxTable {
    join: Sound,
    leave: Sound,
    mute_on: Sound,
    mute_off: Sound,
    deafen_on: Sound,
    deafen_off: Sound,
    send: Sound,
    receive: Sound,
    user_join: Sound,
    user_left: Sound,
    error: Sound,
    call: Sound,
}

impl SfxTable {
    fn decode_all(rate: u32) -> Self {
        Self {
            join: Sound::decode(SOUND_JOIN, rate),
            leave: Sound::decode(SOUND_LEAVE, rate),
            mute_on: Sound::decode(SOUND_MUTE_ON, rate),
            mute_off: Sound::decode(SOUND_MUTE_OFF, rate),
            deafen_on: Sound::decode(SOUND_DEAFEN_ON, rate),
            deafen_off: Sound::decode(SOUND_DEAFEN_OFF, rate),
            send: Sound::decode(SOUND_SEND, rate),
            receive: Sound::decode(SOUND_RECEIVE, rate),
            user_join: Sound::decode(SOUND_USER_JOIN, rate),
            user_left: Sound::decode(SOUND_USER_LEFT, rate),
            error: Sound::decode(SOUND_ERROR, rate),
            call: Sound::decode(SOUND_CALL, rate),
        }
    }
}

/// Named events. Keep in sync with the kit files.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SfxEvent {
    Join,
    Leave,
    MuteOn,
    MuteOff,
    DeafenOn,
    DeafenOff,
    Send,
    Receive,
    UserJoin,
    UserLeft,
    Error,
    Call,
}

// Per-callback scratch buffer (avoid per-block allocations in the audio
// thread).
thread_local! {
    static SCRATCH: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}

impl Sfx {
    /// Boot the output stream. Never panics — on any failure the layer
    /// disables itself (alive = false) and `play` becomes a no-op.
    pub fn new() -> Self {
        // Pre-size audio thread scratch buffer to avoid resize() alloc on first I16 callback.
        SCRATCH.with(|s| s.borrow_mut().reserve(8192));
        let mixer = Arc::new(Mutex::new(Mixer { voices: Vec::new(), kept: Vec::new() }));
        let alive = Arc::new(AtomicBool::new(false));

        let build = cpal::default_host();
        let Some(device) = build.default_output_device() else {
            eprintln!("vox-sfx: no output device — UI sounds disabled");
            return Self::disabled(mixer, alive);
        };
        let Ok(config) = device.default_output_config() else {
            eprintln!("vox-sfx: no default output config — UI sounds disabled");
            return Self::disabled(mixer, alive);
        };
        let sample_rate = config.sample_rate();
        let channels = config.channels() as usize;
        let table = SfxTable::decode_all(sample_rate);
        if channels == 0 {
            return Self::disabled(mixer, alive);
        }

        let mixer_cb = Arc::clone(&mixer);
        let stream_result = match config.sample_format() {
            cpal::SampleFormat::F32 => device.build_output_stream(
                config.into(),
                move |data: &mut [f32], _| {
                    mixer_cb.lock().mix(channels, data);
                },
                |err| eprintln!("vox-sfx: output stream error: {err}"),
                None,
            ),
            cpal::SampleFormat::I16 => device.build_output_stream(
                config.into(),
                move |data: &mut [i16], _| {
                    SCRATCH.with(|scratch| {
                        let mut buf = scratch.borrow_mut();
                        if buf.capacity() < 8192 {
                            let needed = 8192 - buf.capacity();
                            buf.reserve(needed);
                        }
                        buf.resize(data.len(), 0.0);
                        mixer_cb.lock().mix(channels, &mut buf);
                        for (d, f) in data.iter_mut().zip(buf.iter()) {
                            *d = (f * 32767.0).clamp(-32768.0, 32767.0) as i16;
                        }
                    });
                },
                |err| eprintln!("vox-sfx: output stream error: {err}"),
                None,
            ),
            other => {
                eprintln!("vox-sfx: unsupported output format {other:?} — disabled");
                return Self::disabled(mixer, alive);
            }
        };

        match stream_result {
            Ok(stream) => {
                if stream.play().is_err() {
                    eprintln!("vox-sfx: stream play failed — UI sounds disabled");
                    return Self::disabled(mixer, alive);
                }
                alive.store(true, Ordering::SeqCst);
                // Keep the stream alive for the app's lifetime.
                std::mem::forget(stream);
            }
            Err(e) => {
                eprintln!("vox-sfx: build_output_stream failed: {e} — UI sounds disabled");
            }
        }

        Self {
            sounds: Arc::new(table),
            mixer,
            alive,
        }
    }

    fn disabled(mixer: Arc<Mutex<Mixer>>, alive: Arc<AtomicBool>) -> Self {
        Self {
            sounds: Arc::new(SfxTable::decode_all(48_000)),
            mixer,
            alive,
        }
    }

    pub fn play(&self, ev: SfxEvent) {
        if !self.alive.load(Ordering::SeqCst) {
            return;
        }
        let sound = match ev {
            SfxEvent::Join => &self.sounds.join,
            SfxEvent::Leave => &self.sounds.leave,
            SfxEvent::MuteOn => &self.sounds.mute_on,
            SfxEvent::MuteOff => &self.sounds.mute_off,
            SfxEvent::DeafenOn => &self.sounds.deafen_on,
            SfxEvent::DeafenOff => &self.sounds.deafen_off,
            SfxEvent::Send => &self.sounds.send,
            SfxEvent::Receive => &self.sounds.receive,
            SfxEvent::UserJoin => &self.sounds.user_join,
            SfxEvent::UserLeft => &self.sounds.user_left,
            SfxEvent::Error => &self.sounds.error,
            SfxEvent::Call => &self.sounds.call,
        };
        if sound.samples.is_empty() {
            return;
        }
        let mut mixer = self.mixer.lock();
        // Cap simultaneous voices — a burst of events shouldn't pile up.
        if mixer.voices.len() < 8 {
            mixer.voices.push(Voice {
                samples: Arc::clone(&sound.samples),
                pos: 0,
                gain: 0.9,
            });
        }
    }
}

impl Default for Sfx {
    fn default() -> Self {
        Self::new()
    }
}
