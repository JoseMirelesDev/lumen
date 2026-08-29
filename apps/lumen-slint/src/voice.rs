//! VoiceController: owns the framework-agnostic `lumen-voice` VoiceClient and
//! bridges VoiceEvent → Slint properties. Port of the voice store + VoiceView
//! state of the Svelte app; speaking hysteresis lives here (Rust), not in JS.
//!
//! Runs on the host's tokio runtime; the level stream arrives at ~10 Hz.
//!
//! UI thread-safety: `VecModel` is not `Sync`, so the peers model cannot be
//! mutated from the tokio thread. Instead the model is created once on the UI
//! thread (inside `attach`) and kept alive by a UI-thread `Timer`. Every tick
//! the timer copies the shared peer tiles into the *same* `VecModel` via
//! `set_row_data` (per-row `row_changed`), so Slint reuses the tile elements
//! and their animations stay continuous. Rebuilding the model at 10 Hz (as a
//! naive port does) would destroy/recreate every tile every frame, which
//! resets every `animate` and makes the pane look frozen/jumpy.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lumen_core::{ApiClient, CoreEvent, EventBus};
use lumen_voice::{
    dm::{DmDataChannel, DmEvent, DmSession, DmSignalIn, DmSignalOut},
    VoiceClient, VoiceEvent, VoiceJoinArgs,
};
use parking_lot::{Mutex, RwLock};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use crate::sound::{Sfx, SfxEvent};
use crate::{AppWindow, PeerTile};

// Speech thresholds, aligned with the VoiceMeter's silence floor (0.05):
// room-noise RMS (~0.02-0.04) must not trip "speaking". Hysteresis keeps the
// gate from flickering on the threshold — turn on above SPEAK_ON, stay on
// until below SPEAK_OFF.
const SPEAK_ON: f32 = 0.05;
const SPEAK_OFF: f32 = 0.03;

fn with_hysteresis(level: f32, was_speaking: bool) -> bool {
    level > SPEAK_ON || (was_speaking && level > SPEAK_OFF)
}

#[derive(Clone, Default)]
struct PeerState {
    tile: PeerTile,
}

/// Everything needed to re-join the channel after the signaling socket dies
/// (network drop, server cut the idle WS during suspend, ...).
#[derive(Clone)]
struct JoinInfo {
    backend_url: String,
    token: String,
    user_id: String,
    username: String,
    channel_id: String,
    channel_name: String,
}

/// One open DM data-channel session: the channel + its outbound signaling
/// sender (drained by the bridge task in `dm_open`).
#[derive(Clone)]
struct DmPeer {
    channel: Arc<DmDataChannel>,
    signal_tx: tokio::sync::mpsc::UnboundedSender<DmSignalOut>,
    peer: String,
}

pub struct VoiceController {
    client: Arc<VoiceClient>,
    api: Arc<ApiClient>,
    settings: Arc<lumen_core::Settings>,
    rt: tokio::runtime::Handle,
    weak: RwLock<Option<Weak<AppWindow>>>,
    /// Peer tiles, shared with the UI-thread sync timer.
    peers: Arc<Mutex<Vec<PeerState>>>,
    /// Set whenever `peers` changed; consumed by the UI-thread timer.
    peers_dirty: AtomicBool,
    local_level: RwLock<f32>,
    local_speaking: AtomicBool,
    active: AtomicBool,
    muted: AtomicBool,
    deafened: AtomicBool,
    channel_name: RwLock<Option<String>>,
    /// Id del canal de voz en el que estamos (para el indicador in-channel
    /// de la lista de canales).
    channel_id: RwLock<Option<String>>,
    error: RwLock<Option<String>>,
    /// Open P2P DM data-channel (ADR-006): at most one at a time.
    dm: parking_lot::RwLock<Option<DmPeer>>,
    bus: EventBus,
    /// Present while the user is (or wants to be) in the channel; drives the
    /// auto-reconnect loop. Cleared by an explicit leave.
    join_info: RwLock<Option<JoinInfo>>,
    /// One reconnect loop at a time.
    reconnecting: AtomicBool,
    /// UI sound layer (join/leave/user events).
    sfx: Arc<Sfx>,
    mic_test_active: AtomicBool,
    mic_test_level: RwLock<f32>,
    mic_test_stream: parking_lot::Mutex<Option<lumen_voice::audio::MicStream>>,
    mic_test_output: parking_lot::Mutex<Option<lumen_voice::audio::AudioOutput>>,
    // cpal::Stream must be kept alive while playing; stored separately because
    // AudioOutput is Clone (Arc inside) but Stream is not.
    mic_test_output_stream: parking_lot::Mutex<Option<cpal::Stream>>,
    /// Cached computed status string — avoids format! on every push() tick.
    cached_status: RwLock<String>,
    cached_peer_count: AtomicUsize,
    cached_status_active: AtomicBool,
    /// Cached pipeline string — recomputed only when suppressor model or AEC changes.
    cached_pipeline: RwLock<String>,
}

impl VoiceController {
    pub fn new(
        api: Arc<ApiClient>,
        settings: Arc<lumen_core::Settings>,
        rt: tokio::runtime::Handle,
        sfx: Arc<Sfx>,
        bus: EventBus,
    ) -> Arc<Self> {
        let (client, events) = VoiceClient::new();
        // Pre-compute initial pipeline string from the client's defaults.
        let init_model = client.suppressor_model();
        let init_aec = client.aec_enabled();
        let init_model_name = match init_model {
            lumen_voice::audio::SuppressorModel::FastEnhancerM => "FastEnhancer-M",
            lumen_voice::audio::SuppressorModel::FastEnhancerS => "FastEnhancer-S",
            lumen_voice::audio::SuppressorModel::NsOnly => "NS",
        };
        let init_aec_str = if init_aec { "on" } else { "off" };
        let init_pipeline = format!("Pipeline: {} · AEC {} · 48 kHz", init_model_name, init_aec_str);
        let this = Arc::new(Self {
            client: Arc::new(client),
            api,
            settings,
            rt,
            sfx,
            weak: RwLock::new(None),
            peers: Arc::new(Mutex::new(Vec::new())),
            peers_dirty: AtomicBool::new(false),
            local_level: RwLock::new(0.0),
            local_speaking: AtomicBool::new(false),
            active: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            channel_name: RwLock::new(None),
            channel_id: RwLock::new(None),
            error: RwLock::new(None),
            dm: parking_lot::RwLock::new(None),
            bus: bus.clone(),
            join_info: RwLock::new(None),
            reconnecting: AtomicBool::new(false),
            mic_test_active: AtomicBool::new(false),
            mic_test_level: RwLock::new(0.0),
            mic_test_stream: parking_lot::Mutex::new(None),
            mic_test_output: parking_lot::Mutex::new(None),
            mic_test_output_stream: parking_lot::Mutex::new(None),
            cached_status: RwLock::new("not connected".to_string()),
            cached_peer_count: AtomicUsize::new(0),
            cached_status_active: AtomicBool::new(false),
            cached_pipeline: RwLock::new(init_pipeline),
        });
        let drain = Arc::clone(&this);
        let mut rx = events;
        this.rt.spawn(async move {
            while let Some(ev) = rx.recv().await {
                drain.on_event(ev);
            }
        });
        // Apply the persisted suppressor model so the first join uses it.
        if let Some(s) = this.settings.suppressor_model() {
            if let Some(m) = lumen_voice::audio::SuppressorModel::parse(&s) {
                this.client.set_suppressor_model(m);
            }
        }
        // AEC (echo cancellation): persisted per user — speakers on, headphones off.
        // Applied synchronously so the first `push_shell` (and the first join)
        // see the persisted value. The old rt.spawn raced the UI shell: the
        // checkbox could read the default `true` before the async task ran.
        if let Some(enabled) = this.settings.aec_enabled() {
            this.client.set_aec_enabled_now(enabled);
        }
        // Refresh cached pipeline after applying persisted settings.
        {
            let model = this.client.suppressor_model();
            let model_name = match model {
                lumen_voice::audio::SuppressorModel::FastEnhancerM => "FastEnhancer-M",
                lumen_voice::audio::SuppressorModel::FastEnhancerS => "FastEnhancer-S",
                lumen_voice::audio::SuppressorModel::NsOnly => "NS",
            };
            let aec_str = if this.client.aec_enabled() { "on" } else { "off" };
            *this.cached_pipeline.write() = format!("Pipeline: {} · AEC {} · 48 kHz", model_name, aec_str);
        }
        this
    }

    /// Persist + apply the chosen suppressor model. Applies live: the active
    /// session's send path switches on the next frame (no re-join needed).
    pub fn set_suppressor_model(&self, model: lumen_voice::audio::SuppressorModel) {
        self.client.set_suppressor_model(model);
        self.settings.set_suppressor_model(model.as_str().to_string());
        // Recompute cached pipeline string.
        let model_name = match model {
            lumen_voice::audio::SuppressorModel::FastEnhancerM => "FastEnhancer-M",
            lumen_voice::audio::SuppressorModel::FastEnhancerS => "FastEnhancer-S",
            lumen_voice::audio::SuppressorModel::NsOnly => "NS",
        };
        let aec_str = if self.client.aec_enabled() { "on" } else { "off" };
        *self.cached_pipeline.write() = format!("Pipeline: {} · AEC {} · 48 kHz", model_name, aec_str);
    }

    pub fn current_suppressor_model(&self) -> lumen_voice::audio::SuppressorModel {
        self.client.suppressor_model()
    }

    pub fn current_aec_enabled(&self) -> bool {
        self.client.aec_enabled()
    }

    /// Toggle AEC3 (echo cancellation). Speakers users need it; headphones
    /// users should keep it off (this webrtc build corrupts the send when the
    /// render reference is fed).
    pub fn set_aec_enabled(&self, enabled: bool) {
        // Optimistic cache update so the next push() shows the new value immediately.
        {
            let model = self.client.suppressor_model();
            let model_name = match model {
                lumen_voice::audio::SuppressorModel::FastEnhancerM => "FastEnhancer-M",
                lumen_voice::audio::SuppressorModel::FastEnhancerS => "FastEnhancer-S",
                lumen_voice::audio::SuppressorModel::NsOnly => "NS",
            };
            let aec_str = if enabled { "on" } else { "off" };
            *self.cached_pipeline.write() = format!("Pipeline: {} · AEC {} · 48 kHz", model_name, aec_str);
        }
        let client = self.client.clone();
        let settings = self.settings.clone();
        self.rt.spawn(async move {
            client.set_aec_enabled(enabled).await;
            settings.set_aec_enabled(enabled);
        });
    }

    /// Campfire animations (fire frames + seat pulse). Off forces the OS
    /// reduced-motion path: the scene freezes to its static render.
    pub fn animations_enabled(&self) -> bool {
        self.settings.animations_enabled().unwrap_or(true)
    }

    pub fn set_animations_enabled(&self, enabled: bool) {
        self.settings.set_animations_enabled(enabled);
    }

    /// Canal de voz en el que estamos conectados (None si no).
    pub fn current_channel_id(&self) -> Option<String> {
        self.channel_id.read().clone()
    }

    /// Campfire particle/fire frames (AnimationImage). Off hides only the
    /// animated fire; the static brazier glow and seat pulse remain.
    pub fn particles_enabled(&self) -> bool {
        self.settings.particles_enabled().unwrap_or(true)
    }

    pub fn set_particles_enabled(&self, enabled: bool) {
        self.settings.set_particles_enabled(enabled);
    }

    /// Whether the offline mic test is capturing.
    pub fn is_mic_testing(&self) -> bool {
        self.mic_test_active.load(Ordering::SeqCst)
    }

    /// Current RMS level from the mic test (0..1), updated at ~50 Hz.
    pub fn mic_test_level(&self) -> f32 {
        *self.mic_test_level.read()
    }

    /// Start the offline mic test: captures via `start_capture`, runs through
    /// `NoiseSuppressor` with the current model, measures `rms_level`, and
    /// pushes updates. Also plays back the processed audio through the output
    /// device so you hear exactly what would be sent (Discord-style loopback).
    /// When AEC is enabled, the playback render is fed back as `process_render_frame`
    /// (capped at 40 ms per mic frame, like the real send path) — without this
    /// the speaker echo picked up by the mic would Larsen (you heard this).
    /// Does NOT join a voice channel.
    pub fn start_mic_test(self: &Arc<Self>) {
        if self.mic_test_active.swap(true, Ordering::SeqCst) {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<i16>>();
        let stream = match lumen_voice::audio::start_capture(tx) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("mic test start failed: {e}");
                self.mic_test_active.store(false, Ordering::SeqCst);
                return;
            }
        };
        *self.mic_test_stream.lock() = Some(stream);
        // Start loopback playback (best-effort; level meter still works without it).
        // Wiring the render_tap lets AEC see its own playback — speaker mode needs it,
        // headphones mode has aec_enabled=false and skips the feeding.
        let aec_enabled = self.current_aec_enabled();
        let render_tap: std::sync::Arc<parking_lot::Mutex<Vec<i16>>> =
            std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        {
            let mut output = lumen_voice::audio::AudioOutput::new();
            output.set_render_tap(std::sync::Arc::clone(&render_tap));
            match output.start() {
                Ok(cpal_stream) => {
                    *self.mic_test_output.lock() = Some(output);
                    *self.mic_test_output_stream.lock() = Some(cpal_stream);
                }
                Err(e) => eprintln!("mic test playback start failed: {e}"),
            }
        }
        let model = self.current_suppressor_model();
        let this = Arc::clone(self);
        self.rt.spawn(async move {
            let mut suppressor =
                lumen_voice::audio::NoiseSuppressor::with_model_and_aec(model, aec_enabled);
            let mut current_aec = aec_enabled;
            let mut current_model = model;
            let mut diag_last = std::time::Instant::now();
            let mut frame_idx: u64 = 0;
            while let Some(frame) = rx.recv().await {
                if !this.mic_test_active.load(Ordering::SeqCst) {
                    break;
                }
                // Live AEC/model switch (como client.rs) — si toggles AEC en UI
                // mientras mic test corre, debe recrear suppressor sin reiniciar test.
                let live_aec = this.current_aec_enabled();
                let live_model = this.current_suppressor_model();
                if live_aec != current_aec || live_model != current_model {
                    suppressor = lumen_voice::audio::NoiseSuppressor::with_model_and_aec(live_model, live_aec);
                    current_aec = live_aec;
                    current_model = live_model;
                    eprintln!("mic_test: switch aec={} model={:?}", live_aec, live_model.as_str());
                }
                // Feed Sonora AEC render reference — cap 300ms (14400), lockstep 960/480, gate 0.0008
                // Migrado webrtc->sonora pure Rust M145. Cap 150→300 cubre delay 224 (>cap) que causaba
                // Larsen (ERL 4.3 inestable). Ver crates/lumen-voice/src/audio.rs tuning doc
                // para anti_howling 400→200 gain 1.0→0.3 equivalente en sonora_aec3.
                if current_aec {
                    let mut tap = render_tap.lock();
                    const RENDER_CAP: usize = 48_000 * 300 / 1000; // 14400 — cubre delay 224 >150
                    let excess = tap.len().saturating_sub(RENDER_CAP);
                    if excess > 0 {
                        tap.drain(..excess);
                    }
                    let available = (tap.len() / 480) * 480;
                    let to_feed = available.min(960);
                    let tap_before = tap.len();
                    if to_feed > 0 {
                        let render: Vec<i16> = tap.drain(..to_feed).collect();
                        drop(tap);
                        let mut fed = 0;
                        for chunk in render.chunks(480) {
                            let rms = lumen_voice::audio::rms_level(chunk);
                            // gate 0.0008 (-62dB) — bloquea silencio real sin matar voz suave;
                            // 0.002 ya bloqueaba 30% voz, 0.01 bloqueaba voz completa en sonora.
                            if rms > 0.0008 {
                                suppressor.process_render_frame(chunk);
                                fed += 480;
                            }
                        }
                        if diag_last.elapsed().as_secs_f32() >= 2.0 {
                            let stats = suppressor.get_stats();
                            // rms real del frame de captura para diagnóstico nearend dominante
                            // enr_threshold 0.25 puede suprimir voz suave; 0.35 webrtc tuned preserva voz.
                            let in_rms = lumen_voice::audio::rms_level(&frame);
                            eprintln!("mic_test AEC diag tap_before={} fed={} in_rms={:.5} stats delay_ms={:?} erl={:?} erle={:?} nearend_enr=0.25->0.35", tap_before, fed, in_rms, stats.as_ref().and_then(|s| s.delay_ms), stats.as_ref().and_then(|s| s.echo_return_loss), stats.as_ref().and_then(|s| s.echo_return_loss_enhancement));
                            diag_last = std::time::Instant::now();
                        }
                    } else if tap_before > 0 {
                        // tap tiene resto <480, espera próximo frame
                        drop(tap);
                    } else {
                        drop(tap);
                        if frame_idx % 50 == 0 {
                            eprintln!("mic_test AEC diag tap empty (no render yet) frame {}", frame_idx);
                        }
                    }
                }
                let processed = suppressor.process(&frame);
                let level = lumen_voice::audio::rms_level(&processed).clamp(0.0, 1.0);
                *this.mic_test_level.write() = level;
                if let Some(out) = this.mic_test_output.lock().as_ref() {
                    out.push(&processed);
                }
                this.push();
                frame_idx += 1;
            }
            *this.mic_test_level.write() = 0.0;
            this.push();
        });
        self.push();
    }

    /// Stop the offline mic test and drop the capture/playback streams.
    pub fn stop_mic_test(&self) {
        if !self.mic_test_active.swap(false, Ordering::SeqCst) {
            return;
        }
        *self.mic_test_stream.lock() = None;
        *self.mic_test_output_stream.lock() = None;
        *self.mic_test_output.lock() = None;
        *self.mic_test_level.write() = 0.0;
        self.push();
    }

    /// Toggle the offline mic test.
    pub fn toggle_mic_test(self: &Arc<Self>) {
        if self.mic_test_active.load(Ordering::SeqCst) {
            self.stop_mic_test();
        } else {
            self.start_mic_test();
        }
    }

    /// Install the window handle and start the UI-thread model sync timer.
    /// Must be called from the UI thread (controller attach, before run()).
    pub fn attach(self: &Arc<Self>, weak: Weak<AppWindow>) {
        *self.weak.write() = Some(weak.clone());
        let this = Arc::clone(self);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            // Persistent model: the same VecModel is mutated in place on every
            // sync tick (per-row set_row_data), so tile instances are reused
            // and their `animate` transitions keep playing. Structural changes
            // (peer join/leave) go through set_vec, a one-time rebuild.
            let model = Rc::new(VecModel::<PeerTile>::default());
            ui.set_voice_peers(ModelRc::new(model.clone()));
            // Leaked: app-lifetime timer, keeps the model alive on the UI thread.
            let timer: &'static Timer = Box::leak(Box::new(Timer::default()));
            timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
                if !this.peers_dirty.swap(false, Ordering::SeqCst) {
                    return;
                }
                let tiles = this.peers.lock().iter().map(|s| s.tile.clone()).collect::<Vec<_>>();
                let n = model.row_count();
                if tiles.len() == n {
                    for (i, t) in tiles.iter().enumerate() {
                        model.set_row_data(i, t.clone());
                    }
                } else {
                    model.set_vec(tiles);
                }
            });
        });
    }

    /// Resolve ICE servers and join the channel. Idempotent per channel.
    pub async fn join(
        &self,
        backend_url: String,
        token: String,
        user_id: String,
        username: String,
        channel_id: String,
        channel_name: String,
    ) {
        *self.join_info.write() = Some(JoinInfo {
            backend_url: backend_url.clone(),
            token: token.clone(),
            user_id: user_id.clone(),
            username: username.clone(),
            channel_id: channel_id.clone(),
            channel_name: channel_name.clone(),
        });
        if self.active.load(Ordering::SeqCst) {
            return;
        }
        let ice = match self.api.get_realtime_config().await {
            Ok(cfg) => cfg.ice_servers.iter().map(|s| lumen_voice::IceServer {
                urls: match &s.urls {
                    serde_json::Value::String(u) => vec![u.clone()],
                    serde_json::Value::Array(arr) => arr
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect(),
                    _ => vec![],
                },
                username: s.username.clone(),
                credential: s.credential.clone(),
            }).collect(),
            Err(_) => vec![],
        };
        *self.channel_id.write() = Some(channel_id.clone());
        let args = VoiceJoinArgs {
            backend_url,
            token,
            channel_id,
            user_id,
            username,
            ice_servers: ice,
            open_mic: true,
            input_wav: None,
            open_output: true,
        };
        *self.channel_name.write() = Some(channel_name);
        self.peers.lock().clear();
        self.peers_dirty.store(true, Ordering::SeqCst);
        match self.client.join(args).await {
            Ok(()) => {
                self.active.store(true, Ordering::SeqCst);
                *self.error.write() = None;
                self.sfx.play(SfxEvent::Join);
            }
            Err(e) => {
                *self.error.write() = Some(e);
                self.active.store(false, Ordering::SeqCst);
                self.sfx.play(SfxEvent::Error);
            }
        }
        self.push();
    }

    pub async fn leave(&self) {
        *self.join_info.write() = None;
        *self.channel_id.write() = None;
        self.client.leave().await;
        self.active.store(false, Ordering::SeqCst);
        self.sfx.play(SfxEvent::Leave);
        self.peers.lock().clear();
        self.peers_dirty.store(true, Ordering::SeqCst);
        *self.local_level.write() = 0.0;
        self.local_speaking.store(false, Ordering::SeqCst);
        self.push();
    }

    pub async fn toggle_mute(&self) {
        let muted = !self.muted.fetch_xor(true, Ordering::SeqCst);
        self.sfx.play(if muted { SfxEvent::MuteOn } else { SfxEvent::MuteOff });
        self.client.set_muted(muted).await;
        self.push();
    }

    pub async fn toggle_deafen(&self) {
        let deafened = !self.deafened.fetch_xor(true, Ordering::SeqCst);
        self.sfx
            .play(if deafened { SfxEvent::DeafenOn } else { SfxEvent::DeafenOff });
        self.client.set_deafened(deafened).await;
        self.push();
    }

    fn on_event(self: &Arc<Self>, ev: VoiceEvent) {
        match ev {
            VoiceEvent::Levels { local, peers } => {
                // Only reflect levels while actually in a call: the capture
                // stream may outlive a leave() and would otherwise keep the
                // "talking" state lit outside the voice channel.
                if !self.active.load(Ordering::SeqCst) {
                    self.local_speaking.store(false, Ordering::SeqCst);
                    *self.local_level.write() = 0.0;
                    return;
                }
                let was = self.local_speaking.load(Ordering::SeqCst);
                self.local_speaking.store(with_hysteresis(local, was), Ordering::SeqCst);
                *self.local_level.write() = local;
                {
                    let mut list = self.peers.lock();
                    for p in peers {
                        if let Some(state) = list.iter_mut().find(|s| s.tile.id.as_str() == p.peer_id) {
                            state.tile.speaking = with_hysteresis(p.level, state.tile.speaking);
                            state.tile.level = p.level;
                        }
                    }
                } // guard dropped before push() (parking_lot is not reentrant)
                self.peers_dirty.store(true, Ordering::SeqCst);
                self.push();
            }
            VoiceEvent::PeerJoined { peer_id, user_id, username } => {
                let short: String = if username.is_empty() {
                    // Fallback if the peer didn't send a name: last 6 of the id.
                    user_id.chars().rev().take(6).collect::<String>().chars().rev().collect()
                } else {
                    username
                };
                let initial: String = short
                    .chars()
                    .next()
                    .map(|c| c.to_uppercase().collect::<String>())
                    .unwrap_or_default();
                self.sfx.play(SfxEvent::UserJoin);
                {
                    let mut list = self.peers.lock();
                    if !list.iter().any(|s| s.tile.id.as_str() == peer_id) {
                        let skin = crate::model::skin_for(&short);
                        list.push(PeerState {
                            tile: PeerTile {
                                id: peer_id.into(),
                                username: short.into(),
                                initial: initial.into(),
                                level: 0.0,
                                speaking: false,
                                state: "new".into(),
                                muted: false,
                                skin,
                            },
                        });
                    }
                } // guard dropped before push()
                self.peers_dirty.store(true, Ordering::SeqCst);
                self.push();
            }
            VoiceEvent::PeerLeft { peer_id } => {
                self.peers.lock().retain(|s| s.tile.id.as_str() != peer_id);
                self.peers_dirty.store(true, Ordering::SeqCst);
                self.sfx.play(SfxEvent::UserLeft);
                self.push();
            }
            VoiceEvent::State { peer_id, state } => {
                {
                    let mut list = self.peers.lock();
                    if let Some(s) = list.iter_mut().find(|s| s.tile.id.as_str() == peer_id) {
                        s.tile.state = state.as_str().into();
                    }
                } // guard dropped before push()
                self.peers_dirty.store(true, Ordering::SeqCst);
                self.push();
            }
            VoiceEvent::Signaling { state } => {
                self.active.store(false, Ordering::SeqCst);
                if state == lumen_voice::SignalingState::Replaced {
                    // Another connection for this user took over (e.g. a
                    // second window). Do NOT auto-reconnect — it would fight
                    // the new connection in a reconnect loop.
                    *self.join_info.write() = None;
                    *self.error.write() = Some(
                        "Replaced by another connection — join again to take over".into(),
                    );
                } else {
                    self.schedule_reconnect();
                }
                self.push();
            }
            VoiceEvent::Error { code: _, message } => {
                *self.error.write() = Some(message);
                self.push();
            }
            VoiceEvent::Debug { .. } => {}
            // In-call chat from a peer's data channel (ADR-006): route to the
            // bus; the UiController decodes {type, content} frames.
            VoiceEvent::DataChannelMessage { peer_id, data } => {
                self.bus.publish(CoreEvent::InCallChat { peer_id, data });
            }
        }
    }

    /// Re-join the channel with backoff after the signaling socket died
    /// (network drop, server cut an idle WS during suspend, ...). The timers
    /// use tokio's monotonic clock, which does not advance across system
    /// suspend, so a hibernate that outlives a backoff window simply retries
    /// on wake instead of giving up. Stops on an explicit leave.
    fn schedule_reconnect(self: &Arc<Self>) {
        if self.join_info.read().is_none() {
            return;
        }
        if self.reconnecting.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        self.rt.spawn(async move {
            let mut delay = Duration::from_secs(2);
            while this.join_info.read().is_some() {
                tokio::time::sleep(delay).await;
                let Some(info) = this.join_info.read().clone() else { break };
                this.join(
                    info.backend_url,
                    info.token,
                    info.user_id,
                    info.username,
                    info.channel_id,
                    info.channel_name,
                )
                .await;
                if this.active.load(Ordering::SeqCst) {
                    break;
                }
                delay = (delay * 2).min(Duration::from_secs(30));
            }
            this.reconnecting.store(false, Ordering::SeqCst);
        });
    }

    // -- P2P DM data channel (ADR-006, Fase 3) -----------------------------

    /// Open a data-only DM session with an online friend. `send_relay` is the
    /// bridge to the presence WS (`dm-signal`); inbound frames arrive via
    /// `dm_signal`. Emits CoreEvent::InCallChat for inbound messages.
    pub async fn dm_open(
        self: &Arc<Self>,
        peer: String,
        ice: Vec<lumen_voice::IceServer>,
        send_relay: impl Fn(DmSignalOut) + Send + Sync + 'static,
    ) {
        // Close any previous session first (one DM at a time).
        self.dm_close().await;
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel::<DmEvent>();
        let Ok(session) = DmDataChannel::open(&ice, events_tx).await else {
            return;
        };
        let signal_tx = session.signal_tx.clone();
        let channel = session.channel.clone();
        *self.dm.write() = Some(DmPeer { channel, signal_tx: signal_tx.clone(), peer: peer.clone() });
        // Drain outbound signaling → presence relay.
        let this = self.clone();
        self.rt.spawn(async move {
            let mut rx = session.rx;
            while let Some(out) = rx.recv().await {
                send_relay(out);
            }
        });
        // Drain inbound events → CoreEvent.
        let this2 = self.clone();
        self.rt.spawn(async move {
            while let Some(ev) = events_rx.recv().await {
                match ev {
                    DmEvent::Message(data) => {
                        this2.bus_publish(CoreEvent::InCallChat { peer_id: peer.clone(), data });
                    }
                    DmEvent::Open | DmEvent::Closed | DmEvent::Error(_) => {}
                }
            }
        });
        // We are the initiator: send the offer.
        let _ = session.channel.create_offer(&signal_tx).await;
        let _ = this;
    }

    fn bus_publish(&self, ev: CoreEvent) {
        self.bus.publish(ev);
    }

    /// Handle an inbound DM signaling frame relayed by the presence WS.
    pub async fn dm_signal(&self, peer: &str, signal: DmSignalIn) {
        let dm = self.dm.read().clone();
        let Some(dm) = dm else { return };
        if dm.peer != peer {
            return;
        }
        let _ = dm.channel.handle_signal(signal, &dm.signal_tx).await;
    }

    /// Send a text frame over the open DM data channel.
    pub async fn dm_send(&self, peer: &str, text: &str) {
        let dm = self.dm.read().clone();
        let Some(dm) = dm else { return };
        if dm.peer != peer {
            return;
        }
        let _ = dm.channel.send(text).await;
    }

    pub async fn dm_close(&self) {
        let dm = self.dm.write().take();
        if let Some(dm) = dm {
            dm.channel.close().await;
        }
    }

    /// Push the current voice state into the Slint window (from any thread).
    /// The peers model itself is synced by the UI-thread timer in `attach`.
    ///
    /// Deduplicado: se salta el `upgrade_in_event_loop` si nada relevante
    /// cambió (el level stream llega a ~10 Hz; setear una propiedad a su mismo
    /// valor sigue siendo barato en Slint, pero evitar el closure+set evita el
    /// re-render inútil del VoiceMeter cuando el nivel no cruza su umbral).
    fn push(&self) {
        let weak = self.weak.read().clone();
        let Some(weak) = weak else { return };
        let local_level = *self.local_level.read();
        let local_speaking = self.local_speaking.load(Ordering::SeqCst);
        let active = self.active.load(Ordering::SeqCst);
        let muted = self.muted.load(Ordering::SeqCst);
        let deafened = self.deafened.load(Ordering::SeqCst);
        let error = self.error.read().clone().unwrap_or_default();
        let peer_count = self.peers.lock().len();
        let particles = self.particles_enabled();
        let mic_testing = self.mic_test_active.load(Ordering::SeqCst);
        let mic_level = *self.mic_test_level.read();
        // --- cached status: recompute only when active or peer_count changes ---
        let status: SharedString = {
            let prev_active = self.cached_status_active.load(Ordering::Relaxed);
            let prev_count = self.cached_peer_count.load(Ordering::Relaxed);
            if prev_active != active || prev_count != peer_count {
                let new_status = if active {
                    let total = peer_count + 1;
                    match peer_count {
                        0 => "connected — solo en la fogata".to_string(),
                        _ => format!("connected — {total} en la fogata"),
                    }
                } else {
                    "not connected".to_string()
                };
                *self.cached_status.write() = new_status;
                self.cached_status_active.store(active, Ordering::Relaxed);
                self.cached_peer_count.store(peer_count, Ordering::Relaxed);
            }
            SharedString::from(self.cached_status.read().as_str())
        };
        // --- cached pipeline: already maintained by set_suppressor_model / set_aec_enabled ---
        let pipeline_status: SharedString =
            SharedString::from(self.cached_pipeline.read().as_str());
        // Channel name without intermediate String clone — produce SharedString directly.
        let channel_name: SharedString = {
            let guard = self.channel_name.read();
            SharedString::from(guard.as_deref().unwrap_or(""))
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_voice_active(active);
            ui.set_voice_muted(muted);
            ui.set_voice_deafened(deafened);
            ui.set_voice_local_level(local_level);
            ui.set_voice_local_speaking(local_speaking);
            ui.set_voice_channel_name(channel_name);
            ui.set_voice_error(error.into());
            ui.set_voice_status(status);
            ui.set_mic_testing(mic_testing);
            ui.set_mic_test_level(mic_level);
            ui.set_audio_pipeline_status(pipeline_status);
            // El campfire cabalga este render: su mark_dirty_region + request_redraw
            // coalescen con el que ya dispara el level stream, sin renders propios.
            // No-op si el key no está registrado o las partículas están apagadas.
            if !ui.get_reduced_motion() && particles {
                crate::particles::tick_fire(&ui.window());
            }
        });
    }
}
