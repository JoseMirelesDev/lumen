//! UiController — the orchestrator. Owns the shared services (API, auth,
//! shell state, voice) and composes the per-domain controllers that each own
//! a slice of the AppWindow contract:
//!
//! - ctrl::auth::AuthController  — login/register/logout/session bootstrap
//! - ctrl::shell::ShellController — servers, channels, friends, DMs, invite
//! - ctrl::chat::ChatController  — messages, composer, links, previews
//! - src::voice::VoiceController — P2P voice (attached here)
//!
//! This file keeps only what crosses domains: the `push_shell` state fan-out
//! (reads every service, writes every UI property), the voice wiring that
//! reflects host state into the UI, and the shared clipboard + event bus.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use lumen_core::{ApiClient, AuthService, ChannelKind, EventBus, ShellState};
use parking_lot::RwLock;
use slint::{ComponentHandle, Weak};

use crate::ctrl;
use crate::model;
use crate::sound::{Sfx, SfxEvent};
use crate::voice::VoiceController;
use crate::AppWindow;

/// The clipboard handle must live for the whole session: X11 serves the
/// selection lazily from the owning process, so a dropped handle = empty
/// clipboard. arboard's Clipboard is Send but not Sync — the Mutex makes
/// the static sound. Shared by ShellController (invite) and ChatController
/// (message copy).
pub(crate) static CLIPBOARD: LazyLock<Mutex<Option<arboard::Clipboard>>> =
    LazyLock::new(|| Mutex::new(None));

pub struct UiController {
    pub api: Arc<ApiClient>,
    pub auth: Arc<AuthService>,
    pub shell: Arc<ShellState>,
    pub voice: Arc<VoiceController>,
    bus: EventBus,
    pub rt: tokio::runtime::Handle,
    sfx: Arc<Sfx>,
    weak: RwLock<Option<Weak<AppWindow>>>,
    /// Last shell error surfaced to the UI — edge-triggers the error sound.
    last_error: RwLock<String>,
    /// Message ids already surfaced — new incoming ids edge-trigger Receive.
    seen_msgs: parking_lot::RwLock<std::collections::HashSet<String>>,
    /// Whether we're in register mode (auth slice state kept here so the
    /// orchestrator's push can reflect it if needed).
    is_register: AtomicBool,
    /// Toast stack (transient feedback) + monotonic id source.
    toasts: parking_lot::RwLock<Vec<crate::model::ToastItem>>,
    toast_seq: AtomicI32,
    // Per-domain controllers.
    pub auth_ctrl: Arc<ctrl::auth::AuthController>,
    pub shell_ctrl: Arc<ctrl::shell::ShellController>,
    pub chat_ctrl: Arc<ctrl::chat::ChatController>,
}

impl UiController {
    pub fn new(
        api: Arc<ApiClient>,
        auth: Arc<AuthService>,
        shell: Arc<ShellState>,
        voice: Arc<VoiceController>,
        bus: EventBus,
        rt: tokio::runtime::Handle,
        sfx: Arc<Sfx>,
    ) -> Arc<Self> {
        // `on_changed` re-pushes the whole UI; each sub-controller calls it
        // after mutating core state. Built via new_cyclic so the closure can
        // capture a Weak<UiController> before the Arc exists.
        Arc::new_cyclic(|weak_self: &std::sync::Weak<UiController>| {
            // Clone the Weak to an owned value so the closure is 'static.
            let weak_changed: std::sync::Weak<UiController> = weak_self.clone();
            let weak_toast: std::sync::Weak<UiController> = weak_self.clone();
            let on_changed: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                if let Some(c) = weak_changed.upgrade() {
                    c.push();
                }
            });
            let on_toast: Arc<dyn Fn(String, String) + Send + Sync> = Arc::new(move |message, kind| {
                if let Some(c) = weak_toast.upgrade() {
                    c.toast(&message, &kind);
                }
            });
            Self {
                api: api.clone(),
                auth: auth.clone(),
                shell: shell.clone(),
                voice: voice.clone(),
                bus: bus.clone(),
                rt: rt.clone(),
                sfx: sfx.clone(),
                weak: RwLock::new(None),
                last_error: RwLock::new(String::new()),
                seen_msgs: parking_lot::RwLock::new(std::collections::HashSet::new()),
                is_register: AtomicBool::new(false),
                toasts: parking_lot::RwLock::new(Vec::new()),
                toast_seq: AtomicI32::new(1),
                auth_ctrl: ctrl::auth::AuthController::new(
                    api.clone(),
                    auth.clone(),
                    shell.clone(),
                    voice.clone(),
                    bus,
                    rt.clone(),
                    sfx.clone(),
                    on_changed.clone(),
                ),
                shell_ctrl: ctrl::shell::ShellController::new(
                    api.clone(),
                    auth.clone(),
                    shell.clone(),
                    voice.clone(),
                    rt.clone(),
                    on_changed.clone(),
                    on_toast.clone(),
                ),
                chat_ctrl: ctrl::chat::ChatController::new(
                    shell.clone(),
                    rt.clone(),
                    sfx.clone(),
                    on_changed,
                    on_toast,
                ),
            }
        })
    }

    fn weak(&self) -> Weak<AppWindow> {
        self.weak.read().clone().expect("UiController not attached")
    }

    pub fn attach(self: &Arc<Self>, ui: &AppWindow) {
        let weak = ui.as_weak();
        *self.weak.write() = Some(weak.clone());
        self.voice.attach(weak.clone());
        // Sub-controllers register their own callbacks.
        self.auth_ctrl.attach(ui);
        self.shell_ctrl.attach(ui);
        self.chat_ctrl.attach(ui);
        // Presence/real-time events (Fase 3): presence WS → shell + UI.
        let this = self.clone();
        let mut rx = self.bus.subscribe();
        self.rt.spawn(async move {
            while let Ok(ev) = rx.recv().await {
                this.on_core_event(ev);
            }
        });
        // Orchestrator-owned callbacks: voice + model reflection.
        let this = self.clone();
        ui.on_voice_join(move || this.voice_join());
        let this = self.clone();
        ui.on_voice_leave(move || this.voice_leave());
        let this = self.clone();
        ui.on_voice_toggle_mute(move || this.voice_toggle_mute());
        let this = self.clone();
        ui.on_voice_toggle_deafen(move || this.voice_toggle_deafen());
        let this = self.clone();
        ui.on_voice_suppressor_model_changed(move |m| {
            if let Some(model) = lumen_voice::audio::SuppressorModel::parse(m.as_str()) {
                this.voice.set_suppressor_model(model);
                // Reflect the value back into the UI (async-safe via the loop).
                let weak = this.weak();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_voice_suppressor_model(model.as_str().into());
                });
            }
        });
        let this = self.clone();
        ui.on_voice_aec_enabled_changed(move |enabled| {
            this.voice.set_aec_enabled(enabled);
            let weak = this.weak();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_voice_aec_enabled(enabled);
            });
        });
        let this = self.clone();
        ui.on_voice_animations_enabled_changed(move |enabled| {
            this.voice.set_animations_enabled(enabled);
            let weak = this.weak();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_voice_animations_enabled(enabled);
                // Effective reduced-motion = OS preference OR the toggle off.
                ui.set_reduced_motion(os_reduced_motion() || !enabled);
            });
        });
        let this = self.clone();
        ui.on_voice_particles_enabled_changed(move |enabled| {
            this.voice.set_particles_enabled(enabled);
            let weak = this.weak();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_voice_particles_enabled(enabled);
            });
        });
    }

    // -- voice actions (orchestrator-owned; depend on shell + auth + voice) --

    fn voice_join(self: &Arc<Self>) {
        let this = self.clone();
        self.rt.spawn(async move {
            this.shell_ctrl.join_selected_voice().await;
            this.push();
        });
    }

    fn voice_leave(self: &Arc<Self>) {
        let this = self.clone();
        self.rt.spawn(async move {
            this.voice.leave().await;
            this.push();
        });
    }

    fn voice_toggle_mute(self: &Arc<Self>) {
        let this = self.clone();
        self.rt.spawn(async move {
            this.voice.toggle_mute().await;
        });
    }

    fn voice_toggle_deafen(self: &Arc<Self>) {
        let this = self.clone();
        self.rt.spawn(async move {
            this.voice.toggle_deafen().await;
        });
    }

    // -- presence / real-time events (Fase 3) ------------------------------

    fn on_core_event(self: &Arc<Self>, ev: lumen_core::CoreEvent) {
        let shell = &self.shell;
        match ev {
            lumen_core::CoreEvent::PresenceReady { online_friends, servers } => {
                shell.apply_presence_ready(online_friends, servers);
                // Subscribe to the currently open channel (if any).
                if let Some(ch) = shell.selected_channel_id.read().clone() {
                    shell.subscribe_channel(ch);
                }
                self.push();
            }
            lumen_core::CoreEvent::FriendOnline { user_id, username } => {
                shell.apply_friend_online(&user_id, &username);
                self.push();
            }
            lumen_core::CoreEvent::FriendOffline { user_id } => {
                shell.apply_friend_offline(&user_id);
                self.push();
            }
            lumen_core::CoreEvent::FriendStatus { user_id, status } => {
                shell.apply_friend_status(&user_id, status);
                self.push();
            }
            lumen_core::CoreEvent::VoiceOccupancyChanged { server_id, channel_id, peers } => {
                shell.apply_voice_occupancy(&server_id, &channel_id, peers);
                self.push();
            }
            lumen_core::CoreEvent::MemberOnline { server_id, user_id, username } => {
                shell.apply_member_online(&server_id, &user_id, &username);
                self.push();
            }
            lumen_core::CoreEvent::MemberOffline { server_id, user_id } => {
                shell.apply_member_offline(&server_id, &user_id);
                self.push();
            }
            lumen_core::CoreEvent::RealtimeMessage { channel_id, message } => {
                shell.on_realtime_message(&channel_id, message);
                self.push();
            }
            lumen_core::CoreEvent::ChatAck { client_id, .. } => {
                shell.on_chat_ack(&client_id);
                // The confirmed message only lands in `shell.messages` after
                // the reload — push AFTER it completes so the sender's echo
                // appears with its real id + server timestamp. (The old
                // immediate push raced the reload; the pre-fix event-loop
                // spin masked it by pushing constantly.)
                let this = self.clone();
                let shell = Arc::clone(shell);
                self.rt.spawn(async move {
                    shell.load_messages().await;
                    this.push();
                });
            }
            lumen_core::CoreEvent::ChatEditAck { .. } | lumen_core::CoreEvent::ChatDeleteAck { .. } => {
                // Same reasoning as ChatAck: reload first, then push, so the
                // acked mutation (real id/timestamp/soft-delete) is what the
                // UI renders.
                let this = self.clone();
                let shell = Arc::clone(shell);
                self.rt.spawn(async move {
                    shell.load_messages().await;
                    this.push();
                });
            }
            lumen_core::CoreEvent::ChatEdited { channel_id, message } => {
                shell.apply_chat_edited(&channel_id, &message.id, &message.content, &message.edited_at);
                self.push();
            }
            lumen_core::CoreEvent::ChatDeleted { channel_id, message_id } => {
                shell.apply_chat_deleted(&channel_id, &message_id);
                self.push();
            }
            lumen_core::CoreEvent::ChatError { code, .. } => {
                shell.set_error(code);
                self.push();
            }
            lumen_core::CoreEvent::Typing { channel_id, user_id } => {
                self.on_typing(&channel_id, &user_id);
            }
            // The DO confirmed the channel subscription (subscribe-ack).
            // Sender-only bookkeeping: once acked, chat broadcasts for the
            // channel are guaranteed to reach this socket; before the ack a
            // message can be filtered by the server (ordering across sockets
            // isn't guaranteed). Messages missed in that window are recovered
            // from D1 on the next load/scroll — no data loss, eventual
            // consistency by design (ADR-004/005).
            lumen_core::CoreEvent::SubscribeAck { .. } => {}
            // DM signaling relay (ADR-006): bridge presence frames to the
            // voice client's data-only DM session.
            lumen_core::CoreEvent::DmOffer { from, sdp } => {
                let this = self.clone();
                self.rt.spawn(async move {
                    this.voice.dm_signal(&from, lumen_voice::dm::DmSignalIn::Offer { sdp }).await;
                });
            }
            lumen_core::CoreEvent::DmAnswer { from, sdp } => {
                let this = self.clone();
                self.rt.spawn(async move {
                    this.voice.dm_signal(&from, lumen_voice::dm::DmSignalIn::Answer { sdp }).await;
                });
            }
            lumen_core::CoreEvent::DmIce { from, candidate } => {
                let this = self.clone();
                self.rt.spawn(async move {
                    this.voice.dm_signal(&from, lumen_voice::dm::DmSignalIn::Ice { candidate }).await;
                });
            }
            // P2P DM data-channel frames (ADR-006): {type: typing|chat}.
            lumen_core::CoreEvent::InCallChat { peer_id, data } => {
                self.on_in_call_chat(&peer_id, &data);
            }
            // Transient notification (toast) from any service.
            lumen_core::CoreEvent::Toast { message, kind } => {
                self.toast(&message, &kind);
            }
            // Events handled elsewhere (auth/shell/chat controllers).
            _ => {}
        }
    }

    /// A P2P DM data-channel frame arrived (ADR-006): JSON {type, content}.
    fn on_in_call_chat(self: &Arc<Self>, peer_id: &str, data: &[u8]) {
        let Ok(text) = std::str::from_utf8(data) else { return };
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(text) else { return };
        match frame.get("type").and_then(|t| t.as_str()) {
            Some("typing") => {
                // Show the typing indicator on the open DM with this peer.
                let Some(username) = self.username_of(peer_id) else { return };
                let weak = self.weak();
                let name = username.clone();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_typing_label(format!("{name} está escribiendo…").into());
                });
                let this = self.clone();
                self.rt.spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    let weak = this.weak();
                    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_typing_label("".into()));
                });
            }
            Some("chat") => {
                let Some(content) = frame.get("content").and_then(|c| c.as_str()) else { return };
                // Append to the open DM with this peer (local echo of P2P).
                let dm_channel = self.shell.dm_list.read().iter().find(|d| {
                    d.other_username == self.username_of(peer_id).unwrap_or_default()
                }).map(|d| d.channel.id.clone());
                let Some(channel_id) = dm_channel else { return };
                if self.shell.selected_channel_id.read().as_deref() != Some(channel_id.as_str()) {
                    return;
                }
                let username = self.username_of(peer_id).unwrap_or_else(|| "peer".to_string());
                let msg_id = format!("p2p-{}", self.shell.messages.read().len());
                let mut msgs = self.shell.messages.write();
                msgs.push(lumen_core::TextMessage {
                    id: msg_id,
                    channel_id: channel_id.clone(),
                    author_id: peer_id.to_string(),
                    author_name: username,
                    content: content.to_string(),
                    created_at: lumen_core::now_iso(),
                    edited_at: None,
                    deleted_at: None,
                    reply_to: None,
                });
                drop(msgs);
                self.push();
            }
            _ => {}
        }
    }

    fn username_of(&self, user_id: &str) -> Option<String> {
        self.shell.friends.read().iter().find(|f| f.user.id == user_id).map(|f| f.user.username.clone())
    }

    /// Typing indicator: show "X está escribiendo…" for 3s on the active chat.
    fn on_typing(self: &Arc<Self>, channel_id: &str, user_id: &str) {
        if self.shell.selected_channel_id.read().as_deref() != Some(channel_id) {
            return;
        }
        let name = self
            .shell
            .servers
            .read()
            .iter()
            .flat_map(|s| s.channels.iter())
            .find(|c| c.id == channel_id)
            .map(|_| {
                self.shell
                    .presence
                    .server_presence
                    .read()
                    .values()
                    .flat_map(|sp| sp.online_members.iter())
                    .find(|m| m.user_id == user_id)
                    .map(|m| m.username.clone())
                    .unwrap_or_else(|| "Alguien".to_string())
            })
            .unwrap_or_default();
        let weak = self.weak();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_typing_label(if name.is_empty() { "".into() } else { format!("{name} está escribiendo…").into() });
        });
        let this = self.clone();
        let channel = channel_id.to_string();
        self.rt.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            if this.shell.selected_channel_id.read().as_deref() == Some(channel.as_str()) {
                let weak = this.weak();
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_typing_label("".into()));
            }
        });
    }

    // -- toasts ------------------------------------------------------------

    /// Push a transient notification (bottom-center chip, auto-dismiss 4s).
    /// Safe from any thread; the model set goes through the event loop.
    fn toast(self: &Arc<Self>, message: &str, kind: &str) {
        let id = self.toast_seq.fetch_add(1, Ordering::SeqCst);
        {
            let mut toasts = self.toasts.write();
            toasts.push(crate::model::ToastItem {
                id,
                message: message.into(),
                kind: kind.into(),
            });
            while toasts.len() > 4 {
                toasts.remove(0);
            }
        }
        let weak = self.weak();
        {
            let snapshot = self.toasts.read().clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_toasts(crate::model::toasts_model(&snapshot));
            });
        }
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            this.toasts.write().retain(|t| t.id != id);
            let snapshot = this.toasts.read().clone();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_toasts(crate::model::toasts_model(&snapshot));
            });
        });
    }

    // -- push --------------------------------------------------------------

    fn push(self: &Arc<Self>) {
        let weak = self.weak();
        let this = self.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
    }

    fn push_shell(self: &Arc<Self>, ui: &AppWindow) {
        let user = self.auth.user.read().clone();
        let user_id = user.as_ref().map(|u| u.id.clone()).unwrap_or_default();
        let username = user.as_ref().map(|u| u.username.clone()).unwrap_or_default();
        let shell = &self.shell;
        let selected_server_id = shell.selected_server_id.read().clone();
        let selected_channel_id = shell.selected_channel_id.read().clone();
        let servers = shell.servers.read().clone();
        let channels = self.shell_ctrl.current_channels();
        let messages = shell.messages.read().clone();
        let friends = shell.friends.read().clone();
        let pending = shell.pending.read().clone();
        let dm_list = shell.dm_list.read().clone();
        let view = *shell.view.read();
        let dm_call = *shell.dm_call.read();
        let shell_error = shell.error.read().clone().unwrap_or_default();

        ui.set_logged_in(user.is_some());
        ui.set_current_user(username.clone().into());
        ui.set_servers(model::servers_model(&servers, &selected_server_id));
        ui.set_selected_server_id(selected_server_id.clone().unwrap_or_default().into());
        // Fase 3: voice occupancy per channel (peers badge) + online members.
        let mut peers_by_channel = std::collections::HashMap::<String, usize>::new();
        let mut online_members: Vec<lumen_core::PeerLite> = Vec::new();
        if let Some(sid) = selected_server_id.as_ref() {
            if let Some(sp) = shell.presence.server_presence.read().get(sid) {
                for vc in &sp.voice_channels {
                    peers_by_channel.insert(vc.channel_id.clone(), vc.peers.len());
                }
                online_members = sp.online_members.clone();
            }
        }
        ui.set_channels(model::channels_model(
            &channels,
            &selected_channel_id,
            &peers_by_channel,
            &self.voice.current_channel_id(),
        ));
        ui.set_online_members(model::members_model(&online_members));
        ui.set_channel_list_title(
            self.shell_ctrl.current_server().map(|s| s.name).unwrap_or_default().into(),
        );
        ui.set_can_create_channel(
            self.shell_ctrl.current_server().map(|s| s.owner_id == user_id).unwrap_or(false),
        );
        ui.set_messages(model::messages_model(&messages, &user_id));
        self.chat_ctrl.resolve_link_previews(ui);
        ui.set_chat_title(self.shell_ctrl.chat_title().into());
        ui.set_is_dm(self.shell_ctrl.selected_channel_kind() == Some(ChannelKind::Dm));
        ui.set_dm_call(dm_call);
        ui.set_friends_view(view == lumen_core::View::Friends);
        let online_ids: std::collections::HashSet<String> = shell
            .presence
            .online_friends
            .read()
            .keys()
            .cloned()
            .collect();
        ui.set_friends_online(model::friends_online_model(&friends, &online_ids));
        ui.set_friends_offline(model::friends_offline_model(&friends, &online_ids));
        ui.set_friend_requests(model::requests_model(&pending));
        ui.set_dms(model::dms_model(&dm_list));
        // Edge-triggered sounds: error (new non-empty shell error) and
        // incoming message (id not seen before, not authored by us).
        {
            let mut last = self.last_error.write();
            if shell_error != *last {
                if !shell_error.is_empty() {
                    self.sfx.play(SfxEvent::Error);
                    // Todo error (avatar, red, chat, permisos…) también va al
                    // toast — el bar persistente sigue, el toast da el aviso
                    // inmediato.
                    self.toast(&shell_error, "error");
                }
                *last = shell_error.clone();
            }
        }
        {
            let mut seen = self.seen_msgs.write();
            for m in &messages {
                if seen.insert(m.id.clone()) && m.author_id != user_id {
                    self.sfx.play(SfxEvent::Receive);
                }
            }
        }
        ui.set_chat_error(shell_error.clone().into());
        ui.set_friends_error(shell_error.clone().into());
        ui.set_voice_visible(self.shell_ctrl.voice_visible());
        ui.set_voice_local_user(username.clone().into());
        ui.set_local_skin(model::skin_for(&username));
        let animations = self.voice.animations_enabled();
        ui.set_reduced_motion(os_reduced_motion() || !animations);
        ui.set_voice_animations_enabled(animations);
        ui.set_voice_particles_enabled(self.voice.particles_enabled());
        ui.set_voice_suppressor_model(self.voice.current_suppressor_model().as_str().into());
        // Non-silent fallback: tell the UI whether this CPU can run the
        // FastEnhancer-M engine, so a degraded-to-NS selection is surfaced.
        ui.set_voice_suppressor_model_available(lumen_voice::audio::FastEnhancerDenoiser::available());
        ui.set_voice_aec_enabled(self.voice.current_aec_enabled());
    }
}

/// OS-level "reduce motion" preference. Linux: GNOME enable-animations via
/// gsettings (one cheap exec). Other platforms default to motion-on; extend
/// when a native accessibility API is needed.
/// OS-level "reduce motion" preference, read ONCE off the UI thread and
/// cached. `gsettings` can hang indefinitely (no session bus / dconf), and
/// running `Command::output()` synchronously on the UI thread blocked the
/// whole client. Falls back to motion-on when unavailable.
fn read_os_reduced_motion_blocking() -> bool {
    #[cfg(target_os = "linux")]
    {
        // Hard timeout: even the warm thread must never hang forever.
        let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<std::process::Output>>();
        std::thread::spawn(move || {
            let out = std::process::Command::new("gsettings")
                .args(["get", "org.gnome.desktop.interface", "enable-animations"])
                .output();
            let _ = tx.send(out);
        });
        match rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(Ok(out)) => !String::from_utf8_lossy(&out.stdout).trim().contains("true"),
            _ => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

// 0 = motion on, 1 = reduced, 2 = not read yet
static OS_REDUCED_MOTION: AtomicU8 = AtomicU8::new(2);

/// Kick the background read; call once at startup (never on the UI thread).
pub(crate) fn warm_os_reduced_motion() {
    std::thread::spawn(|| {
        let reduced = read_os_reduced_motion_blocking();
        OS_REDUCED_MOTION.store(if reduced { 1 } else { 0 }, Ordering::Relaxed);
    });
}

/// Cached OS reduced-motion; false (motion on) until the warm read lands.
pub(crate) fn os_reduced_motion() -> bool {
    OS_REDUCED_MOTION.load(Ordering::Relaxed) == 1
}
