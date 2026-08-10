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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lumen_core::ApiClient;
use lumen_voice::{VoiceClient, VoiceEvent, VoiceJoinArgs};
use parking_lot::{Mutex, RwLock};
use slint::{Model, ModelRc, Timer, TimerMode, VecModel, Weak};

use crate::{AppWindow, PeerTile};

const SPEAK_ON: f32 = 0.03;
const SPEAK_OFF: f32 = 0.02;

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
    error: RwLock<Option<String>>,
    /// Present while the user is (or wants to be) in the channel; drives the
    /// auto-reconnect loop. Cleared by an explicit leave.
    join_info: RwLock<Option<JoinInfo>>,
    /// One reconnect loop at a time.
    reconnecting: AtomicBool,
}

impl VoiceController {
    pub fn new(
        api: Arc<ApiClient>,
        settings: Arc<lumen_core::Settings>,
        rt: tokio::runtime::Handle,
    ) -> Arc<Self> {
        let (client, events) = VoiceClient::new();
        let this = Arc::new(Self {
            client: Arc::new(client),
            api,
            settings,
            rt,
            weak: RwLock::new(None),
            peers: Arc::new(Mutex::new(Vec::new())),
            peers_dirty: AtomicBool::new(false),
            local_level: RwLock::new(0.0),
            local_speaking: AtomicBool::new(false),
            active: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            channel_name: RwLock::new(None),
            error: RwLock::new(None),
            join_info: RwLock::new(None),
            reconnecting: AtomicBool::new(false),
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
        this
    }

    /// Persist + apply the chosen suppressor model (takes effect on the next
    /// join; the active session keeps its current model).
    pub fn set_suppressor_model(&self, model: lumen_voice::audio::SuppressorModel) {
        self.client.set_suppressor_model(model);
        self.settings.set_suppressor_model(model.as_str().to_string());
    }

    pub fn current_suppressor_model(&self) -> lumen_voice::audio::SuppressorModel {
        self.client.suppressor_model()
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
        let args = VoiceJoinArgs {
            backend_url,
            token,
            channel_id,
            user_id,
            username,
            ice_servers: ice,
        };
        *self.channel_name.write() = Some(channel_name);
        self.peers.lock().clear();
        self.peers_dirty.store(true, Ordering::SeqCst);
        match self.client.join(args).await {
            Ok(()) => {
                self.active.store(true, Ordering::SeqCst);
                *self.error.write() = None;
            }
            Err(e) => {
                *self.error.write() = Some(e);
                self.active.store(false, Ordering::SeqCst);
            }
        }
        self.push();
    }

    pub async fn leave(&self) {
        *self.join_info.write() = None;
        self.client.leave().await;
        self.active.store(false, Ordering::SeqCst);
        self.peers.lock().clear();
        self.peers_dirty.store(true, Ordering::SeqCst);
        *self.local_level.write() = 0.0;
        self.local_speaking.store(false, Ordering::SeqCst);
        self.push();
    }

    pub async fn toggle_mute(&self) {
        let muted = !self.muted.fetch_xor(true, Ordering::SeqCst);
        self.client.set_muted(muted).await;
        self.push();
    }

    pub async fn toggle_deafen(&self) {
        let deafened = !self.deafened.fetch_xor(true, Ordering::SeqCst);
        self.client.set_deafened(deafened).await;
        self.push();
    }

    fn on_event(self: &Arc<Self>, ev: VoiceEvent) {
        match ev {
            VoiceEvent::Levels { local, peers } => {
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
                {
                    let mut list = self.peers.lock();
                    if !list.iter().any(|s| s.tile.id.as_str() == peer_id) {
                        list.push(PeerState {
                            tile: PeerTile {
                                id: peer_id.into(),
                                username: short.into(),
                                initial: initial.into(),
                                level: 0.0,
                                speaking: false,
                                state: "new".into(),
                                muted: false,
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

    /// Push the current voice state into the Slint window (from any thread).
    /// The peers model itself is synced by the UI-thread timer in `attach`.
    fn push(&self) {
        let weak = self.weak.read().clone();
        let Some(weak) = weak else { return };
        let local_level = *self.local_level.read();
        let local_speaking = self.local_speaking.load(Ordering::SeqCst);
        let active = self.active.load(Ordering::SeqCst);
        let muted = self.muted.load(Ordering::SeqCst);
        let deafened = self.deafened.load(Ordering::SeqCst);
        let channel_name = self.channel_name.read().clone().unwrap_or_default();
        let error = self.error.read().clone().unwrap_or_default();
        let peer_count = self.peers.lock().len();
        let status = if active {
            format!("connected — {peer_count} peer(s)")
        } else {
            "not connected".to_string()
        };
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_voice_active(active);
            ui.set_voice_muted(muted);
            ui.set_voice_deafened(deafened);
            ui.set_voice_local_level(local_level);
            ui.set_voice_local_speaking(local_speaking);
            ui.set_voice_channel_name(channel_name.into());
            ui.set_voice_error(error.into());
            ui.set_voice_status(status.into());
        });
    }
}
