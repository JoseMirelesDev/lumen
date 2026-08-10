//! Voice session orchestration: signaling relay × per-peer WebRTC × audio.
//!
//! The whole stack is native:
//! ```text
//! cpal mic ─▶ opus ─▶ RTP ─▶ TrackLocalStaticRTP ─▶ webrtc-rs ─▶ SRTP ─▶ UDP
//! UDP ─▶ webrtc-rs ─▶ TrackRemote ─▶ jitter buffer ─▶ opus ─▶ cpal out
//! both ends negotiated over the WS relay (docs/protocol.md §1)
//! ```
//!
//! Framework-agnostic: the only output is [`VoiceEvent`] on a channel the host
//! owns (see [`VoiceClient::new`]). No Tauri, no UI.
//!
//! Screen/video is deliberately absent. Every peer here is a single audio
//! track; the negotiation path (`add_track`, `remove_track`, re-offer) is the
//! seam where a video `TrackLocalStaticRTP` slots in later, unchanged.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use rtc::peer_connection::configuration::{RTCConfigurationBuilder, RTCIceServer};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::{
    RTCIceGatheringState, RTCPeerConnectionState, RTCSignalingState,
};
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCPeerConnectionIceEvent,
};
use webrtc::rtp_transceiver::RtpSender;

use crate::audio::{
    rms_level, AudioOutput, NetEqOpusDecoder, OpusEncoder, FRAME_SAMPLES,
};
use crate::event::{PeerLevel, PeerState, SignalingState, VoiceEvent};
use crate::rtp::AudioPacketizer;
use crate::signaling::{SignalEvent, SignalOut, SignalingClient};

use neteq::{AudioPacket, NetEq, NetEqConfig, RtpHeader};

/// STUN/TURN server as the frontend supplies it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

/// Arguments for `join`, as the host sends them (camelCase JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceJoinArgs {
    /// HTTP base of the worker, e.g. `https://lumen.example.workers.dev`.
    pub backend_url: String,
    pub token: String,
    pub channel_id: String,
    pub user_id: String,
    /// Display name, sent on join so peers can show it (not just an ID).
    pub username: String,
    #[serde(default)]
    pub ice_servers: Vec<IceServer>,
    /// Open the local mic and send our audio. Set false for a receive-only
    /// participant (e.g. a distant peer in a netns latency test) so it does
    /// not fight a local sender for the same input device.
    #[serde(default = "default_true")]
    pub open_mic: bool,
    /// Feed a WAV file instead of the live mic (real recorded speech, looped).
    /// When set, the mic capture device is NOT opened; frames are read from
    /// this 48 kHz mono i16 WAV and pushed through the send path, so transport
    /// quality can be verified against real intelligible audio.
    #[serde(default)]
    pub input_wav: Option<String>,
    /// Open the audio output device (playback). Set false for a headless
    /// peer (e.g. inside a network namespace with no audio device): NetEQ
    /// still decodes and the diag still measures loss/jitter, but nothing is
    /// played.
    #[serde(default = "default_true")]
    pub open_output: bool,
}

fn default_true() -> bool {
    true
}

/// Diagnostics: `LUMEN_VOICE_DIAG=1` appends a JSONL trace (send RTP, recv RTP,
/// NetEQ per-tick stats) to `<tmp>/lumen-voice-diag-<pid>.jsonl` so a real
/// two-machine call can be diagnosed (packet loss / jitter / PLC expansion).
mod diag {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::sync::{LazyLock, Mutex};

    static F: LazyLock<Mutex<std::fs::File>> = LazyLock::new(|| {
        let path =
            std::env::temp_dir().join(format!("lumen-voice-diag-{}.jsonl", std::process::id()));
        Mutex::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("open diag file"),
        )
    });

    pub fn enabled() -> bool {
        std::env::var("LUMEN_VOICE_DIAG").is_ok()
    }

    pub fn log(ev: &str, obj: &serde_json::Value) {
        if !enabled() {
            return;
        }
        let mut o = obj.clone();
        o["ev"] = serde_json::json!(ev);
        o["t_ms"] = serde_json::json!(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        );
        if let Ok(mut f) = F.lock() {
            let _ = writeln!(f, "{o}");
        }
    }
}

/// How the local mic decides when to transmit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransmitMode {
    /// Always transmit (current behavior).
    Always,
    /// Transmit only while RNNoise detects voice (VAD gate).
    VoiceActivated,
}

// ---------------------------------------------------------------------------
// VoiceClient: framework-agnostic handle. The host owns the event receiver.
// ---------------------------------------------------------------------------

pub struct VoiceClient {
    events: mpsc::UnboundedSender<VoiceEvent>,
    session: tokio::sync::Mutex<Option<Arc<VoiceSession>>>,
    /// Noise-suppression model: the initial model for a join AND the live
    /// model watched by the send task (mid-call switches apply immediately).
    suppressor_model: Arc<parking_lot::RwLock<crate::audio::SuppressorModel>>,
    /// AEC3 (echo cancellation) for the next join. Default true (speakers);
    /// headphones users disable it — this webrtc build corrupts the send when
    /// the render reference is fed.
    aec_enabled: Arc<AtomicBool>,
}

impl VoiceClient {
    /// Create a client and hand back the event stream the host must drain
    /// (adapters bridge it to the UI: Tauri re-emits `voice://*`, Slint
    /// updates properties).
    pub fn new() -> (Self, mpsc::UnboundedReceiver<VoiceEvent>) {
        let (events, rx) = mpsc::unbounded_channel();
        (
            Self {
                events,
                session: tokio::sync::Mutex::new(None),
                // Default: the full-band FastEnhancer tier when the CPU can run
                // it, else NS-only (auto-selection inside `with_model`).
                suppressor_model: Arc::new(parking_lot::RwLock::new(
                    crate::audio::SuppressorModel::FastEnhancerM,
                )),
                aec_enabled: Arc::new(AtomicBool::new(true)),
            },
            rx,
        )
    }

    /// Select the noise-suppression model. Applies live: the active session's
    /// send task swaps its suppressor on the next frame (streaming state
    /// resets, reconverging in a few hundred ms); a future join uses it as
    /// the initial model.
    pub fn set_suppressor_model(&self, model: crate::audio::SuppressorModel) {
        *self.suppressor_model.write() = model;
    }

    pub fn suppressor_model(&self) -> crate::audio::SuppressorModel {
        *self.suppressor_model.read()
    }

    pub async fn join(&self, args: VoiceJoinArgs) -> std::result::Result<(), String> {
        // A previous session may have died on its own — the signaling socket
        // dropped (network, server eviction) and the run_loop exited without
        // an explicit leave(). The slot would otherwise block every re-join
        // and break auto-reconnect, which retries forever with
        // "already in a voice channel".
        let stale = {
            let guard = self.session.lock().await;
            match guard.as_ref() {
                Some(s) => s.run_ended().await,
                None => false,
            }
        };
        if stale {
            *self.session.lock().await = None;
        }
        let mut guard = self.session.lock().await;
        if guard.is_some() {
            return Err("already in a voice channel".into());
        }
        let aec = self.aec_enabled.load(Ordering::SeqCst);
        let session = VoiceSession::start(self.events.clone(), args, self.suppressor_model.clone(), aec)
            .await
            .map_err(|e| e.to_string())?;
        *guard = Some(session);
        Ok(())
    }

    pub async fn leave(&self) {
        if let Some(session) = self.session.lock().await.take() {
            session.stop().await;
        }
    }

    pub async fn set_muted(&self, muted: bool) {
        if let Some(s) = self.session.lock().await.as_ref() {
            s.muted.store(muted, Ordering::SeqCst);
        }
    }

    pub async fn set_deafened(&self, deafened: bool) {
        if let Some(s) = self.session.lock().await.as_ref() {
            s.deafened.store(deafened, Ordering::SeqCst);
            s.output.set_enabled(!deafened);
        }
    }

    /// Set how the mic decides to transmit (always vs voice-activated).
    pub async fn set_transmit_mode(&self, mode: TransmitMode) {
        if let Some(s) = self.session.lock().await.as_ref() {
            s.transmit_mode.store(mode as u8, Ordering::SeqCst);
        }
    }

    /// Toggle feeding the playback (far-end) reference into AEC3.
    ///
    /// Default true (preserves echo cancellation for speaker users). This
    /// webrtc-audio-processing build corrupts the send when any render is fed
    /// (measured: near-end correlation 0.34-0.85 vs 1.0 with no render), so
    /// headphones users should disable it for a clean send. Applies to the
    /// live session and to the next join.
    pub async fn set_aec_enabled(&self, enabled: bool) {
        self.aec_enabled.store(enabled, Ordering::SeqCst);
        if let Some(s) = self.session.lock().await.as_ref() {
            s.aec_enabled.store(enabled, Ordering::SeqCst);
        }
    }

    /// Store the AEC flag synchronously (startup: applies the persisted
    /// setting before any session exists, so the first join and the UI both
    /// see it). Live toggles still go through `set_aec_enabled`.
    pub fn set_aec_enabled_now(&self, enabled: bool) {
        self.aec_enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn aec_enabled(&self) -> bool {
        self.aec_enabled.load(Ordering::SeqCst)
    }

    /// TEMP DIAG: (playout buffer occupancy in frames, total shed samples).
    pub async fn output_stats(&self) -> Option<(usize, u64)> {
        self.session.lock().await.as_ref().map(|s| s.output.stats())
    }
}

// ---------------------------------------------------------------------------
// VoiceSession
// ---------------------------------------------------------------------------

struct VoiceSession {
    events: mpsc::UnboundedSender<VoiceEvent>,
    peers: Arc<Mutex<HashMap<String, Peer>>>,
    output: AudioOutput,
    _mic_stream: crate::audio::MicStream,
    _out_stream: Option<cpal::Stream>,
    muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    /// `TransmitMode` as u8 (0 = Always, 1 = VoiceActivated).
    transmit_mode: Arc<AtomicU8>,
    /// Whether the playback reference is fed into AEC3 (speakers: true,
    /// headphones: false — see `set_aec_enabled`).
    aec_enabled: Arc<AtomicBool>,
    _encoder: Arc<tokio::sync::Mutex<OpusEncoder>>,
    ice: Vec<RTCIceServer>,
    /// The outbound signaling handle. Dropping it closes the WS → loop exits.
    signal_tx: Mutex<Option<mpsc::UnboundedSender<SignalOut>>>,
    stopping: Arc<AtomicBool>,
    /// Handle of the signaling run_loop task; awaited in stop() so the session
    /// (and its cpal output stream) is fully dropped before a re-join starts.
    run: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl VoiceSession {
    async fn start(
        events: mpsc::UnboundedSender<VoiceEvent>,
        args: VoiceJoinArgs,
        suppressor_model: Arc<parking_lot::RwLock<crate::audio::SuppressorModel>>,
        aec_enabled: bool,
    ) -> Result<Arc<Self>> {
        let ws_base = args.backend_url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        let ws_url = format!("{ws_base}/api/ws/{}", args.channel_id);
        let (signal, signal_rx) =
            SignalingClient::connect(
                &ws_url,
                &args.token,
                &args.channel_id,
                &args.user_id,
                &args.username,
            )
            .await
            .context("signaling connect")?;
        let signal_tx = signal.tx.clone();

        let (mic_tx, mut mic_rx) = mpsc::unbounded_channel::<Vec<i16>>();
        // Precedence: input_wav (real recorded speech, looped) > open_mic.
        // Receive-only participant (open_mic=false): skip capture so it does
        // not open the local input device (a netns latency-test peer, or a
        // second session that would fight a local sender for the mic).
        let mic_stream = if let Some(wav) = &args.input_wav {
            let path = wav.clone();
            let frames = crate::audio::load_wav_pcm(&path)?;
            eprintln!("lumen voice: feeding {} ({} frames) instead of mic", path, frames.len());
            tokio::spawn(async move {
                let mut i = 0usize;
                let mut last = std::time::Instant::now();
                while mic_tx.send(
                    frames[i * crate::audio::FRAME_SAMPLES..(i + 1) * crate::audio::FRAME_SAMPLES].to_vec(),
                ).is_ok() {
                    // pace at 20 ms cadence
                    let target = last + std::time::Duration::from_millis(20);
                    let now = std::time::Instant::now();
                    if now < target {
                        tokio::time::sleep(target - now).await;
                    }
                    last = std::time::Instant::now();
                    i += 1;
                    if (i + 1) * crate::audio::FRAME_SAMPLES > frames.len() {
                        i = 0; // loop
                    }
                }
            });
            crate::audio::MicStream::Inactive
        } else if args.open_mic {
            crate::audio::start_capture(mic_tx).context("mic capture")?
        } else {
            crate::audio::MicStream::Inactive
        };
        let _ = &mic_stream; // held alive (Raw keeps the WASAPI client + thread; cpal keeps the stream)
        let mut output = AudioOutput::new();
        // AEC reference: the send path drains this and feeds it to AEC3.
        let render_tap: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::with_capacity(48_000)));
        output.set_render_tap(render_tap.clone());
        // Headless peer (open_output=false, e.g. in a netns with no audio
        // device): keep the mixer running for diag (NetEQ loss/jitter stats)
        // but skip opening the playback device. AudioOutput::push is a no-op
        // without a started stream.
        let out_stream = if args.open_output {
            Some(output.start().context("audio output")?)
        } else {
            eprintln!("lumen voice: headless (no audio output)");
            None
        };

        let peers = Arc::new(Mutex::new(HashMap::<String, Peer>::new()));
        let muted = Arc::new(AtomicBool::new(false));
        let deafened = Arc::new(AtomicBool::new(false));
        // AEC on/off for this session (see `set_aec_enabled`).
        let aec_enabled = Arc::new(AtomicBool::new(aec_enabled));
        // Default to Always transmit: the RNNoise VAD gate (VoiceActivated) is
        // not reliable enough yet — it cuts real speech frames, so the remote
        // hears nothing while the local meter still shows "speaking". The
        // SpeechLeveler below already gates gain (not transmission), so the
        // mic is always audible. VoiceActivated stays available via
        // `set_transmit_mode` once its threshold is calibrated (UI phase).
        let transmit_mode = Arc::new(AtomicU8::new(TransmitMode::Always as u8));
        let local_level = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let encoder = Arc::new(tokio::sync::Mutex::new(OpusEncoder::new()?));
        let stopping = Arc::new(AtomicBool::new(false));
        let ice: Vec<RTCIceServer> = args
            .ice_servers
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone().unwrap_or_default(),
                credential: s.credential.clone().unwrap_or_default(),
            })
            .collect();

        // Send: encode mic frames and fan out to every peer's send worker.
        //
        // The mic hardware drives the cadence — no ticker needed. cpal
        // delivers one 960-sample (20 ms) frame per callback, resampled and
        // buffered by `feed()`. This task just awaits each frame, encodes,
        // and fans out. Using the mic as the clock (instead of a tokio
        // interval) avoids two-clock drift: a software timer and the audio
        // hardware inevitably slip against each other, causing either frame
        // drops (Skip) or stale-audio backlog (Burst).
        {
            let peers = peers.clone();
            let muted = muted.clone();
            let encoder = encoder.clone();
            let local_level = local_level.clone();
            let stopping = stopping.clone();
            let render_tap = render_tap.clone();
            let aec_enabled = aec_enabled.clone();
            tokio::spawn(async move {
                // WebRTC APM (AEC3/HPF/NS) + the selected denoiser tier
                // (FastEnhancer by default, auto-degrading to NS-only). The
                // model is watched so the UI ComboBox applies live, not on
                // re-join (see the per-frame check below).
                let mut current_model = *suppressor_model.read();
                let mut ns = crate::audio::NoiseSuppressor::with_model(current_model);
                let mut frame_idx = 0u64;
                let mut loop_exit = None;
                while let Some(mut frame) = mic_rx.recv().await {
                    // Live model switch: applies immediately, not on re-join.
                    // Recreating the suppressor resets its streaming state
                    // (AEC3/FE reconverge in a few hundred ms — an acceptable
                    // blip on a user-initiated toggle). Drop-then-create on
                    // this single thread is safe for the FastEnhancer C global
                    // engine (fe_free then fe_init).
                    let m = *suppressor_model.read();
                    if m != current_model {
                        ns = crate::audio::NoiseSuppressor::with_model(m);
                        current_model = m;
                    }
                    if stopping.load(Ordering::SeqCst) {
                        loop_exit = Some("stopping");
                        break;
                    }
                    if muted.load(Ordering::SeqCst) {
                        continue;
                    }
                    // AEC3 render reference: fed when the user wants echo
                    // cancellation (speakers: ON; headphones: OFF). The
                    // earlier "render corrupts send" measurement (corr
                    // 0.34-0.85) that motivated the AEC-off default was
                    // contaminated by the frame-shed bug — now fixed by the
                    // threshold-gated shed below — so feeding render is safe.
                    // The tap is drained bounded so it can't grow unbounded.
                    let render: Vec<i16> = {
                        let mut tap = render_tap.lock();
                        const RENDER_CAP: usize = 48_000 / 2; // 500 ms
                        let excess = tap.len().saturating_sub(RENDER_CAP);
                        if excess > 0 {
                            tap.drain(..excess);
                        }
                        std::mem::take(&mut *tap)
                    };
                    if aec_enabled.load(Ordering::SeqCst) {
                        for chunk in render.chunks(480) {
                            ns.process_render_frame(chunk);
                        }
                    }
                    // Shed only on genuine overload: >4 queued frames (>80 ms
                    // backlog). USB capture delivers 40 ms bursts — two
                    // 960-sample frames per ALSA callback, pushed back-to-back
                    // by `feed()` (audio.rs); an unconditional drain pops the
                    // second frame and overwrites the first unsent, halving
                    // the send rate to ~24 pkt/s in production and making the
                    // remote hear robotic audio (NetEQ 96.7% concealment). In
                    // steady state the channel holds 0-1 frames, so this is a
                    // no-op; it only sheds when processing genuinely falls
                    // behind, bounding the backlog at ~100 ms.
                    while mic_rx.len() > 4 {
                        match mic_rx.try_recv() {
                            Ok(f) => frame = f,
                            Err(_) => break,
                        }
                    }
                    local_level.store(rms_level(&frame).to_bits(), Ordering::SeqCst);
                    // Always-transmit (Discord/Krisp-style): `process_gated`
                    // runs the full chain (AEC3 + denoiser + leveler + limiter)
                    // and ALWAYS returns `Some(cleaned)` — every denoised frame
                    // is encoded and sent. DeepFilterNet strips noise
                    // spectrally (its output on a quiet room is near-zero), so
                    // transmitting silence costs nothing audible; a binary
                    // gate caused hard cuts at speech edges. Opus DTX makes
                    // the silence frames ~5-byte packets, so listening-heavy
                    // calls cost negligible bandwidth/CPU. `speech_detected`
                    // still drives the speaking meter.
                    let Some(cleaned) = ns.process_gated(&frame) else {
                        // Unreachable today (process_gated always returns
                        // Some); kept as a defensive no-op.
                        continue;
                    };
                    if diag::enabled() && frame_idx % 25 == 0 {
                        diag::log(
                            "proc",
                            &serde_json::json!({
                                "in_rms": rms_level(&frame),
                                "out_rms": rms_level(&cleaned),
                                "model": current_model.as_str(),
                                "fe": ns.neural_available(),
                                "aec": aec_enabled.load(Ordering::SeqCst),
                            }),
                        );
                    }
                    frame_idx += 1;
                    let encoded = match encoder.lock().await.encode(&cleaned) {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    for peer in peers.lock().values() {
                        let _ = peer.send_tx.send(encoded.clone());
                    }
                }
                // If we get here, the mic channel closed OR stopping was set.
                diag::log(
                    "send_exit",
                    &serde_json::json!({
                        "reason": loop_exit.unwrap_or("mic_rx_closed"),
                        "frames": frame_idx,
                    }),
                );
            });
        }

        // Levels: emit ~10 Hz so the UI can drive metering.
        {
            let peers = peers.clone();
            let local_level = local_level.clone();
            let events = events.clone();
            let stopping = stopping.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_millis(100));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    ticker.tick().await;
                    let local = f32::from_bits(local_level.load(Ordering::SeqCst));
                    let list: Vec<PeerLevel> = peers
                        .lock()
                        .iter()
                        .map(|(id, p)| PeerLevel {
                            peer_id: id.clone(),
                            level: f32::from_bits(p.level.load(Ordering::SeqCst)),
                        })
                        .collect();
                    let _ = events.send(VoiceEvent::Levels { local, peers: list });
                }
            });
        }

        // Playout (session-wide mixer): one 20 ms clock drives every peer's
        // jitter buffer, decoding and summing all frames into a single mixed
        // frame pushed to the shared output. This keeps the output ring at
        // ~1 frame occupancy regardless of how many peers are connected — a
        // per-peer playout (each pushing its own 20 ms frame per tick) used to
        // saturate the shared ring and shed half the audio whenever 2+ peers
        // were connected, and a dropped peer's silence kept eating the buffer
        // for the whole time its connection lingered.
        {
            let peers = peers.clone();
            let output = output.clone();
            let stopping = stopping.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_millis(20));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut acc = vec![0i32; FRAME_SAMPLES];
                let mut mixed = vec![0i16; FRAME_SAMPLES];
                let silence = vec![0i16; FRAME_SAMPLES];
                loop {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    ticker.tick().await;
                    acc.fill(0);
                    let mut talking = 0usize;
                    let mut n_expand = 0u32;
                    let mut n_normal = 0u32;
                    let mut n_cng = 0u32;
                    let mut n_other = 0u32;
                    // Clone handles out of the lock so decoding never holds
                    // the peers map (signaling/send tasks lock it too).
                    let snapshot: Vec<Peer> = peers.lock().values().cloned().collect();
                    for peer in &snapshot {
                        // Pull two 10 ms frames from NetEQ = one 20 ms frame
                        // (960 samples). NetEQ reorders, conceals loss and
                        // adapts its delay, so a frame is always available; a
                        // peer with nothing yet contributes silence.
                        let mut pcm = vec![0i16; FRAME_SAMPLES];
                        let mut got = 0usize;
                        for _ in 0..2 {
                            match peer.neteq.lock().get_audio() {
                                Ok(audio) => {
                                    match audio.speech_type {
                                        neteq::neteq::SpeechType::Expand => n_expand += 1,
                                        neteq::neteq::SpeechType::Cng => n_cng += 1,
                                        neteq::neteq::SpeechType::Normal => n_normal += 1,
                                        _ => n_other += 1,
                                    }
                                    let n = audio.samples.len().min(FRAME_SAMPLES - got);
                                    for (i, s) in audio.samples[..n].iter().enumerate() {
                                        pcm[got + i] = (*s * 32767.0)
                                            .round()
                                            .clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                                    }
                                    got += n;
                                }
                                Err(_) => break,
                            }
                        }
                        if got == 0 {
                            continue; // peer idle: contributes silence
                        }
                        peer.level.store(rms_level(&pcm[..got]).to_bits(), Ordering::SeqCst);
                        for (i, s) in pcm[..got].iter().enumerate() {
                            acc[i] = acc[i].saturating_add(*s as i32);
                        }
                        talking += 1;
                    }
                    if diag::enabled() {
                        if let Some(nq) = snapshot.iter().next().and_then(|p| p.neteq.try_lock()) {
                            let st = nq.get_statistics();
                            diag::log(
                                "mix",
                                &serde_json::json!({
                                    "expand": n_expand,
                                    "normal": n_normal,
                                    "cng": n_cng,
                                    "other": n_other,
                                    "buffer_ms": st.current_buffer_size_ms,
                                    "target_ms": st.target_delay_ms,
                                    "pps": st.packets_per_sec,
                                    "waiting_ms": st.network.mean_waiting_time_ms,
                                }),
                            );
                        }
                    }
                    if talking == 0 {
                        output.push(&silence);
                    } else {
                        for (i, a) in acc.iter().enumerate() {
                            mixed[i] = (*a).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                        }
                        // Several loud peers at once can push the sum past full
                        // scale; brickwall-limit the mix so it can't distort
                        // (clipping distortion is what Opus would otherwise
                        // re-encode on the send side, too).
                        crate::audio::limit_peaks(&mut mixed, 1.0);
                        output.push(&mixed);
                    }
                }
            });
        }

        let session = Arc::new(Self {
            events,
            peers,
            output,
            _mic_stream: mic_stream,
            _out_stream: out_stream,
            muted,
            deafened,
            transmit_mode,
            aec_enabled,
            _encoder: encoder,
            ice,
            signal_tx: Mutex::new(Some(signal_tx)),
            stopping,
            run: tokio::sync::Mutex::new(None),
        });

        let loop_session = Arc::clone(&session);
        *session.run.lock().await = Some(tokio::spawn(async move {
            loop_session.run_loop(signal_rx).await;
        }));
        Ok(session)
    }

    /// Drive the signaling loop until the session closes (stop() or socket end).
    async fn run_loop(self: Arc<Self>, mut signal_rx: mpsc::UnboundedReceiver<SignalEvent>) {
        let mut replaced = false;
        loop {
            tokio::select! {
                event = signal_rx.recv() => {
                    let Some(event) = event else { break };
                    match event {
                        SignalEvent::Closed { replaced: r } => {
                            replaced = r;
                            break;
                        }
                        _ => self.handle_event(event).await,
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if self.stopping.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        }
        self.teardown(replaced).await;
    }

    async fn handle_event(&self, event: SignalEvent) {
        // Re-fetch the current outbound handle each event (it may be dropped).
        let signal_tx = self.signal_tx.lock().clone();
        let Some(signal_tx) = signal_tx else { return };
        match event {
            SignalEvent::Joined { peer_id, peers } => {
                let _ = self.events.send(VoiceEvent::Debug {
                    peer_id: None,
                    message: format!("joined channel as peer {peer_id}"),
                });
                // Existing peers will offer to us; announce them for the UI.
                for p in peers {
                    let _ = self.events.send(VoiceEvent::PeerJoined {
                        peer_id: p.peer_id,
                        user_id: p.user_id,
                        username: p.username,
                    });
                }
            }
            SignalEvent::PeerJoined(p) => {
                let _ = self.events.send(VoiceEvent::PeerJoined {
                    peer_id: p.peer_id.clone(),
                    user_id: p.user_id.clone(),
                    username: p.username.clone(),
                });
                if !self.peers.lock().contains_key(&p.peer_id) {
                    let peer = match self.create_peer(&p.peer_id, &p.user_id, &signal_tx).await {
                        Ok(peer) => peer,
                        Err(err) => {
                            diag::log("peer_err", &serde_json::json!({"where": "create_peer", "err": err.to_string()}));
                            let _ = self.events.send(VoiceEvent::Error {
                                code: None,
                                message: err.to_string(),
                            });
                            return;
                        }
                    };
                    // We are the existing peer: offer to the newcomer.
                    let offer = match peer.pc.create_offer(None).await {
                        Ok(o) => o,
                        Err(err) => {
                            diag::log("peer_err", &serde_json::json!({"where": "create_offer", "err": err.to_string()}));
                            let _ = self.events.send(VoiceEvent::Error {
                                code: None,
                                message: err.to_string(),
                            });
                            return;
                        }
                    };
                    if peer.pc.set_local_description(offer.clone()).await.is_ok() {
                        diag::log("offer_sent", &serde_json::json!({"to": p.peer_id}));
                        let _ = signal_tx.send(SignalOut::Offer {
                            to: p.peer_id.clone(),
                            sdp: offer.sdp,
                        });
                    } else {
                        diag::log("peer_err", &serde_json::json!({"where": "set_local(offer)", "to": p.peer_id}));
                    }
                    self.peers.lock().insert(p.peer_id.clone(), peer);
                }
            }
            SignalEvent::Offer { from, sdp } => self.handle_offer(&signal_tx, &from, sdp).await,
            SignalEvent::Answer { from, sdp } => self.handle_answer(&from, sdp).await,
            SignalEvent::IceCandidate { from, candidate } => self.handle_ice(&from, candidate).await,
            SignalEvent::PeerLeft(peer_id) => self.remove_peer(&peer_id).await,
            SignalEvent::Error { code, message } => {
                let _ = self.events.send(VoiceEvent::Error { code: Some(code), message });
            }
            SignalEvent::Closed { .. } => {}
        }
    }

    /// Create a peer connection for `peer_id`. The caller decides who offers.
    async fn create_peer(
        &self,
        peer_id: &str,
        user_id: &str,
        signal_tx: &mpsc::UnboundedSender<SignalOut>,
    ) -> Result<Peer> {
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;
        let config = RTCConfigurationBuilder::new().with_ice_servers(self.ice.clone()).build();

        let ssrc: u32 = rand::random();
        let track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
            format!("lumen-stream-{peer_id}"),
            format!("lumen-audio-{peer_id}"),
            "microphone".to_string(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: RTCRtpCodec {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: "useinbandfec=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                ..Default::default()
            }],
        )));

        // Per-peer NetEQ: adaptive jitter buffer + decoder. 48 kHz mono, delay
        // clamped (max 200 ms) so a jitter burst can't balloon latency to
        // seconds; min 20 ms keeps it tight when the network is calm.
        let neteq = {
            let mut n = NetEq::new(NetEqConfig {
                sample_rate: 48000,
                channels: 1,
                max_delay_ms: 200,
                min_delay_ms: 20,
                ..Default::default()
            })
            .map_err(anyhow::Error::msg)?;
            n.register_decoder(111, Box::new(NetEqOpusDecoder::new()?));
            n
        };
        let neteq = Arc::new(Mutex::new(neteq));
        let level = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let stop = Arc::new(AtomicBool::new(false));

        let handler = Arc::new(PeerHandler {
            peer_id: peer_id.to_string(),
            signal: signal_tx.clone(),
            events: self.events.clone(),
            neteq: neteq.clone(),
            stop: stop.clone(),
        });

        let runtime = webrtc::runtime::default_runtime().expect("runtime-tokio feature");
        // Bind the peer's UDP socket to a specific interface IP when set
        // (LUMEN_BIND_ADDR, e.g. "10.77.0.2" inside the netns latency test).
        // Default 0.0.0.0:0 generates a host candidate of 0.0.0.0, which is
        // NOT connectable across a veth/netns — the peers only ever saw the
        // identical srflx (same public NAT) and ICE failed. Binding to the
        // real interface IP makes the host candidate reachable.
        let bind = std::env::var("LUMEN_BIND_ADDR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|ip| format!("{ip}:0"))
            .unwrap_or_else(|| "0.0.0.0:0".to_string());
        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(config)
                .with_media_engine(media_engine)
                .with_interceptor_registry(registry)
                .with_handler(handler)
                .with_runtime(runtime)
                .with_udp_addrs(vec![bind])
                .build()
                .await?,
        );
        let sender = pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;

        let (send_tx, send_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        spawn_send_worker(track, sender.clone(), ssrc, send_rx, stop.clone());

        Ok(Peer {
            peer_id: peer_id.to_string(),
            user_id: user_id.to_string(),
            pc,
            sender,
            send_tx,
            neteq,
            level,
            stop,
        })
    }

    async fn handle_offer(
        &self,
        signal_tx: &mpsc::UnboundedSender<SignalOut>,
        from: &str,
        sdp: String,
    ) {
        // Create the peer if the offer precedes any peer-joined message.
        if !self.peers.lock().contains_key(from) {
            match self.create_peer(from, "", signal_tx).await {
                Ok(peer) => {
                    self.peers.lock().insert(from.to_string(), peer);
                }
                Err(err) => {
                    let _ = self.events.send(VoiceEvent::Error {
                        code: None,
                        message: err.to_string(),
                    });
                    return;
                }
            }
        }
        let Some(peer) = self.peers.lock().get(from).cloned() else {
            diag::log("peer_err", &serde_json::json!({"where": "handle_offer no-peer", "from": from}));
            return;
        };
        diag::log("offer_recv", &serde_json::json!({"from": from}));
        let Ok(desc) = RTCSessionDescription::offer(sdp) else {
            diag::log("peer_err", &serde_json::json!({"where": "parse offer", "from": from}));
            return;
        };
        if peer.pc.set_remote_description(desc).await.is_err() {
            diag::log("peer_err", &serde_json::json!({"where": "set_remote(offer)", "from": from}));
            return;
        }
        match peer.pc.create_answer(None).await {
            Ok(answer) => {
                if peer.pc.set_local_description(answer.clone()).await.is_ok() {
                    diag::log("answer_sent", &serde_json::json!({"to": from}));
                    let _ = signal_tx.send(SignalOut::Answer { to: from.to_string(), sdp: answer.sdp });
                } else {
                    diag::log("peer_err", &serde_json::json!({"where": "set_local(answer)", "to": from}));
                }
            }
            Err(err) => {
                let _ = self.events.send(VoiceEvent::Error {
                    code: None,
                    message: err.to_string(),
                });
            }
        }
    }

    async fn handle_answer(&self, from: &str, sdp: String) {
        let Some(peer) = self.peers.lock().get(from).cloned() else {
            let _ = self.events.send(VoiceEvent::Error {
                code: None,
                message: format!("answer from unknown peer {from}"),
            });
            return;
        };
        diag::log("answer_recv", &serde_json::json!({"from": from}));
        let Ok(desc) = RTCSessionDescription::answer(sdp) else {
            diag::log("peer_err", &serde_json::json!({"where": "parse answer", "from": from}));
            return;
        };
        let _ = peer.pc.set_remote_description(desc).await;
    }

    async fn handle_ice(&self, from: &str, candidate: serde_json::Value) {
        let Some(peer) = self.peers.lock().get(from).cloned() else {
            diag::log("ice_dropped", &serde_json::json!({"from": from, "reason": "no-peer-yet"}));
            return; // ICE may precede SDP in trickle mode; peer is created on offer.
        };
        diag::log("ice_recv", &serde_json::json!({"from": from}));
        // JS peers send RTCIceCandidate.toJSON(). candidate + sdpMLineIndex parse;
        // sdpMid/usernameFragment degrade to None, which ICE accepts.
        match serde_json::from_value::<webrtc::peer_connection::RTCIceCandidateInit>(candidate) {
            Ok(init) => {
                if let Err(err) = peer.pc.add_ice_candidate(init).await {
                    let _ = self.events.send(VoiceEvent::Error {
                        code: None,
                        message: err.to_string(),
                    });
                }
            }
            Err(_) => {}
        }
    }

    async fn remove_peer(&self, peer_id: &str) {
        // Bind the guard to its own statement so it is dropped before we await.
        let removed = self.peers.lock().remove(peer_id);
        if let Some(peer) = removed {
            peer.stop.store(true, Ordering::SeqCst);
            let _ = peer.pc.close().await;
            let _ = self.events.send(VoiceEvent::PeerLeft { peer_id: peer_id.to_string() });
        }
    }

    async fn teardown(&self, replaced: bool) {
        self.stopping.store(true, Ordering::SeqCst);
        let peers: Vec<Peer> = self.peers.lock().drain().map(|(_, p)| p).collect();
        for peer in peers {
            peer.stop.store(true, Ordering::SeqCst);
            let _ = peer.pc.close().await;
        }
        let state = if replaced { SignalingState::Replaced } else { SignalingState::Closed };
        let _ = self.events.send(VoiceEvent::Signaling { state });
    }

    /// True once the signaling loop has exited on its own (socket death,
    /// server eviction) and the session is no longer usable. `run` is always
    /// Some after start; None (never started) counts as ended.
    async fn run_ended(&self) -> bool {
        self.run.lock().await.as_ref().map_or(true, |h| h.is_finished())
    }

    /// End the session. Closes peers and the WS; the loop breaks and tears down.
    async fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        // Explicitly ask the signaling writer to close the socket: dropping
        // the sender alone would NOT close it (the heartbeat task holds a
        // clone of the outbound channel, so the writer never sees it close),
        // leaving a ghost peer in the DO map until the next re-join dedup.
        if let Some(tx) = self.signal_tx.lock().take() {
            let _ = tx.send(SignalOut::Close);
        }
        let peers: Vec<Peer> = self.peers.lock().drain().map(|(_, p)| p).collect();
        for peer in peers {
            peer.stop.store(true, Ordering::SeqCst);
            let _ = peer.pc.close().await;
        }
        self.output.set_enabled(false);
        // Block until the signaling loop has exited and dropped the session,
        // so the old cpal output stream is gone before a re-join starts.
        // Otherwise a fast leave→join keeps the previous stream draining its
        // residual buffer, which plays seconds of stale audio.
        if let Some(handle) = self.run.lock().await.take() {
            let _ = handle.await;
        }
    }
}

// ---------------------------------------------------------------------------
// Peer + per-peer tasks
// ---------------------------------------------------------------------------

struct Peer {
    peer_id: String,
    user_id: String,
    pc: Arc<dyn PeerConnection>,
    #[allow(dead_code)]
    sender: Arc<dyn RtpSender>,
    send_tx: mpsc::UnboundedSender<Vec<u8>>,
    /// Inbound adaptive jitter buffer + decoder (NetEQ). Filled by the
    /// per-peer receive task (`insert_packet`), drained by the session-wide
    /// playout task (`get_audio`, two 10 ms frames per 20 ms tick).
    neteq: Arc<Mutex<NetEq>>,
    level: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
}

impl Clone for Peer {
    fn clone(&self) -> Self {
        Peer {
            peer_id: self.peer_id.clone(),
            user_id: self.user_id.clone(),
            pc: self.pc.clone(),
            sender: self.sender.clone(),
            send_tx: self.send_tx.clone(),
            neteq: self.neteq.clone(),
            level: self.level.clone(),
            stop: self.stop.clone(),
        }
    }
}

/// Resolve the payload type negotiated for the sender's (single) codec.
async fn negotiated_payload_type(sender: &Arc<dyn RtpSender>) -> Option<u8> {
    sender.get_parameters().await.ok()?.rtp_parameters.codecs.first().map(|c| c.payload_type)
}

/// Per-peer send worker: packetize encoded frames and write them to the track.
fn spawn_send_worker(
    track: Arc<TrackLocalStaticRTP>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let mut packetizer = AudioPacketizer::new(ssrc, 111);
        let mut pt: Option<u8> = None;
        let mut last_send: Option<std::time::Instant> = None;
        while let Some(frame) = rx.recv().await {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if pt.is_none() {
                if let Some(p) = negotiated_payload_type(&sender).await {
                    packetizer.payload_type = p;
                    pt = Some(p);
                } else {
                    continue; // not negotiated yet — wait for a later frame
                }
            }
            let pkt = packetizer.packet(&frame);
            let now = std::time::Instant::now();
            diag::log(
                "send",
                &serde_json::json!({
                    "seq": pkt.header.sequence_number,
                    "ts": pkt.header.timestamp,
                    "len": pkt.payload.len(),
                    "since_ms": last_send.map(|t| now.duration_since(t).as_millis() as u64).unwrap_or(0),
                }),
            );
            last_send = Some(now);
            let _ = track.write_rtp(pkt).await; // errors while unconnected are dropped
        }
    });
}

// ---------------------------------------------------------------------------
// Per-peer event handler
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct PeerHandler {
    peer_id: String,
    signal: mpsc::UnboundedSender<SignalOut>,
    events: mpsc::UnboundedSender<VoiceEvent>,
    neteq: Arc<Mutex<NetEq>>,
    stop: Arc<AtomicBool>,
}

impl From<RTCPeerConnectionState> for PeerState {
    fn from(s: RTCPeerConnectionState) -> Self {
        match s {
            RTCPeerConnectionState::New => PeerState::New,
            RTCPeerConnectionState::Connecting => PeerState::Connecting,
            RTCPeerConnectionState::Connected => PeerState::Connected,
            RTCPeerConnectionState::Disconnected => PeerState::Disconnected,
            RTCPeerConnectionState::Failed => PeerState::Failed,
            RTCPeerConnectionState::Closed => PeerState::Closed,
            _ => PeerState::Unknown,
        }
    }
}

#[async_trait]
impl PeerConnectionEventHandler for PeerHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        diag::log("ice_local", &serde_json::json!({
            "candidate": event.candidate.to_string(),
        }));
        if let Ok(init) = event.candidate.to_json() {
            // Match the JS RTCIceCandidate.toJSON() shape so JS peers parse it.
            let _ = self.signal.send(SignalOut::IceCandidate {
                to: self.peer_id.clone(),
                candidate: serde_json::json!({
                    "candidate": init.candidate,
                    "sdpMid": init.sdp_mid,
                    "sdpMLineIndex": init.sdp_mline_index,
                    "usernameFragment": init.username_fragment,
                }),
            });
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.events.send(VoiceEvent::Debug {
                peer_id: Some(self.peer_id.clone()),
                message: "ice gathering complete".to_string(),
            });
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        diag::log("conn_state", &serde_json::json!({"peer": self.peer_id, "state": format!("{state:?}")}));
        let _ = self.events.send(VoiceEvent::State {
            peer_id: self.peer_id.clone(),
            state: PeerState::from(state),
        });
    }

    async fn on_signaling_state_change(&self, _state: RTCSignalingState) {}

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        // Receiver: feed inbound RTP into this peer's NetEQ jitter buffer.
        // Playout is session-wide (see the mixer task in VoiceSession::start):
        // it pulls two 10 ms frames per 20 ms tick and mixes them.
        let recv_neteq = self.neteq.clone();
        let stop_recv = self.stop.clone();
        tokio::spawn(async move {
            let mut last_seq: Option<u16> = None;
            let mut last_t: Option<std::time::Instant> = None;
            while let Some(event) = track.poll().await {
                if stop_recv.load(Ordering::SeqCst) {
                    break;
                }
                match event {
                    TrackRemoteEvent::OnRtpPacket(pkt) => {
                        let seq = pkt.header.sequence_number;
                        let ts = pkt.header.timestamp;
                        let now = std::time::Instant::now();
                        diag::log(
                            "recv",
                            &serde_json::json!({
                                "seq": seq,
                                "ts": ts,
                                "gap": last_seq.map(|s| (seq.wrapping_sub(s) as i32).saturating_sub(1).max(0)).unwrap_or(0),
                                "since_ms": last_t.map(|t| now.duration_since(t).as_millis() as u64).unwrap_or(0),
                                "buffer_ms": recv_neteq.lock().get_statistics().current_buffer_size_ms,
                            }),
                        );
                        last_seq = Some(seq);
                        last_t = Some(now);
                        let header = RtpHeader {
                            sequence_number: seq,
                            timestamp: ts,
                            ssrc: pkt.header.ssrc,
                            payload_type: pkt.header.payload_type,
                            marker: pkt.header.marker,
                        };
                        let _ = recv_neteq.lock().insert_packet(AudioPacket::new(
                            header,
                            pkt.payload.to_vec(),
                            48000, // sample rate
                            1,     // channels
                            20,    // one 20 ms OPUS frame per packet
                        ));
                    }
                    TrackRemoteEvent::OnEnded | TrackRemoteEvent::OnEnding | TrackRemoteEvent::OnError => {
                        break;
                    }
                    _ => {}
                }
            }
        });
    }
}
