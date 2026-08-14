//! ShellController — navigation + servers/channels/friends/DMs slice of the
//! AppWindow contract. Owns the ShellState interactions (select/create/join,
//! friends, DMs, invite copy) and the shared chat-title / voice-visible
//! derivations. Voice join on voice-channel select is handled here too (it
//! depends on shell selection + auth identity).

use std::sync::Arc;

use lumen_core::{ApiClient, AuthService, ChannelKind, ShellState, View};
use parking_lot::RwLock;
use slint::{ComponentHandle, Weak};

use crate::voice::VoiceController;
use crate::AppWindow;

pub struct ShellController {
    pub api: Arc<ApiClient>,
    pub auth: Arc<AuthService>,
    pub shell: Arc<ShellState>,
    pub voice: Arc<VoiceController>,
    pub rt: tokio::runtime::Handle,
    weak: RwLock<Option<Weak<AppWindow>>>,
    /// Channel id armed by "rename"/"delete" (consumed by the confirm flows).
    pending_channel: RwLock<Option<String>>,
    /// Injected by UiController: re-push the whole UI after a state change.
    on_changed: Arc<dyn Fn() + Send + Sync>,
    /// Injected by UiController: transient notification (message, kind).
    on_toast: Arc<dyn Fn(String, String) + Send + Sync>,
}

impl ShellController {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api: Arc<ApiClient>,
        auth: Arc<AuthService>,
        shell: Arc<ShellState>,
        voice: Arc<VoiceController>,
        rt: tokio::runtime::Handle,
        on_changed: Arc<dyn Fn() + Send + Sync>,
        on_toast: Arc<dyn Fn(String, String) + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            api,
            auth,
            shell,
            voice,
            rt,
            weak: RwLock::new(None),
            pending_channel: RwLock::new(None),
            on_changed,
            on_toast,
        })
    }

    fn weak(&self) -> Weak<AppWindow> {
        self.weak.read().clone().expect("ShellController not attached")
    }

    pub fn attach(self: &Arc<Self>, ui: &AppWindow) {
        *self.weak.write() = Some(ui.as_weak());
        self.wire(ui);
        // Las solicitudes de amistad no se broadcastan por WS (solo se ven
        // en el próximo load_friends). Mientras la vista Friends está abierta,
        // refresca amigos + solicitudes (sin DMs) cada 20s para que las
        // solicitudes entrantes aparezcan sin recargar. Un GET /api/friends
        // por tick — D1 directo, NO toca el PresenceHubDO (que solo atiende
        // el WS). Fuera de la vista: cero tráfico.
        let this = self.clone();
        self.rt.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(20));
            loop {
                tick.tick().await;
                if *this.shell.view.read() == View::Friends {
                    this.shell.refresh_friends().await;
                    (this.on_changed)();
                }
            }
        });
    }

    fn wire(self: &Arc<Self>, ui: &AppWindow) {
        let this = self.clone();
        ui.on_select_server(move |id| this.select_server(id.to_string()));
        let this = self.clone();
        ui.on_create_server(move |name| this.create_server(name.to_string()));
        let this = self.clone();
        ui.on_join_server(move |code| this.join_server(code.to_string()));
        let this = self.clone();
        ui.on_open_friends(move || this.open_friends());
        let this = self.clone();
        ui.on_select_channel(move |id| this.select_channel(id.to_string()));
        let this = self.clone();
        ui.on_create_channel(move |name, kind| {
            this.create_channel(name.to_string(), kind.to_string());
        });
        let this = self.clone();
        ui.on_copy_invite(move || this.copy_invite());
        let this = self.clone();
        ui.on_open_dm(move |username| this.open_dm(username.to_string()));
        let this = self.clone();
        ui.on_accept_friend(move |id| this.accept_friend(id.to_string()));
        let this = self.clone();
        ui.on_decline_friend(move |id| this.decline_friend(id.to_string()));
        let this = self.clone();
        ui.on_add_friend(move |username| this.add_friend(username.to_string()));
        // Fase 2 — CRUD actions
        let this = self.clone();
        ui.on_server_action(move |action| this.server_action(action.to_string()));
        let this = self.clone();
        ui.on_channel_action(move |action, id| this.channel_action(action.to_string(), id.to_string()));
        let this = self.clone();
        ui.on_remove_friend(move |id| this.remove_friend(id.to_string()));
        let this = self.clone();
        ui.on_username_submit(move |u| this.username_submit(u.to_string()));
        let this = self.clone();
        ui.on_password_submit(move |c, n| this.password_submit(c.to_string(), n.to_string()));
        let this = self.clone();
        ui.on_avatar_submit(move |p| this.avatar_submit(p.to_string()));
        let this = self.clone();
        ui.on_avatar_pick(move || this.avatar_pick());
        let this = self.clone();
        ui.on_member_action(move |action, id| this.member_action(action.to_string(), id.to_string()));
    }

    /// Kick / ban a member (owner only, Fase 5).
    fn member_action(self: &Arc<Self>, action: String, user_id: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            let Some(server_id) = this.shell.selected_server_id.read().clone() else { return };
            let result = match action.as_str() {
                "kick" => this.shell.kick_member(server_id, user_id).await,
                "ban" => this.shell.ban_member(server_id, user_id, None).await,
                _ => return,
            };
            if let Err(e) = result {
                this.shell.set_error(e);
            }
            (this.on_changed)();
        });
    }

    // -- Fase 2 CRUD flows ------------------------------------------------

    fn server_action(self: &Arc<Self>, action: String) {
        let this = self.clone();
        let weak = this.weak();
        match action.as_str() {
            "menu" => {
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_overlay("server-menu".into()));
            }
            "rename" => {
                let name = this.current_server().map(|s| s.name).unwrap_or_default();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_server_rename_text(name.into());
                    ui.set_overlay("server-rename".into());
                });
            }
            "invite" => {
                this.copy_invite();
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_overlay("none".into()));
            }
            "leave" => {
                let server_id = this.shell.selected_server_id.read().clone();
                self.rt.spawn(async move {
                    if let Some(id) = server_id {
                        if let Err(e) = this.shell.leave_server(id).await {
                            this.shell.set_error(e);
                        } else {
                            (this.on_toast)("Servidor abandonado".into(), "info".into());
                        }
                    }
                    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_overlay("none".into()));
                    (this.on_changed)();
                });
            }
            "delete" => {
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_overlay("server-delete".into()));
            }
            "delete-confirm" => {
                let server_id = this.shell.selected_server_id.read().clone();
                self.rt.spawn(async move {
                    if let Some(id) = server_id {
                        if let Err(e) = this.shell.delete_server(id).await {
                            this.shell.set_error(e);
                        } else {
                            (this.on_toast)("Servidor eliminado".into(), "info".into());
                        }
                    }
                    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_overlay("none".into()));
                    (this.on_changed)();
                });
            }
            "rename-submit" => {
                let server_id = this.shell.selected_server_id.read().clone();
                let name = weak
                    .upgrade()
                    .map(|ui| ui.get_server_rename_text().to_string())
                    .unwrap_or_default();
                self.rt.spawn(async move {
                    if let (Some(id), Some(name)) = (server_id, Some(name)) {
                        let name = name.trim().to_string();
                        if !name.is_empty() {
                            if let Err(e) = this.shell.update_server(id, name).await {
                                this.shell.set_error(e);
                            } else {
                                (this.on_toast)("Servidor renombrado".into(), "success".into());
                            }
                        }
                    }
                    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_overlay("none".into()));
                    (this.on_changed)();
                });
            }
            _ => {}
        }
    }

    fn channel_action(self: &Arc<Self>, action: String, id: String) {
        let this = self.clone();
        let weak = this.weak();
        match action.as_str() {
            "rename" => {
                *this.pending_channel.write() = Some(id.clone());
                let name = this
                    .shell
                    .servers
                    .read()
                    .iter()
                    .flat_map(|s| s.channels.iter())
                    .find(|c| c.id == id)
                    .map(|c| c.name.clone())
                    .unwrap_or_default();
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    ui.set_channel_rename_text(name.into());
                    ui.set_overlay("channel-rename".into());
                });
            }
            "delete" => {
                *this.pending_channel.write() = Some(id);
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_overlay("channel-delete".into()));
            }
            "delete-confirm" => {
                let id = this.pending_channel.write().take();
                self.rt.spawn(async move {
                    if let Some(id) = id {
                        if let Err(e) = this.shell.delete_channel(id).await {
                            this.shell.set_error(e);
                        } else {
                            (this.on_toast)("Canal eliminado".into(), "info".into());
                        }
                    }
                    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_overlay("none".into()));
                    (this.on_changed)();
                });
            }
            "rename-submit" => {
                let name = weak
                    .upgrade()
                    .map(|ui| ui.get_channel_rename_text().to_string())
                    .unwrap_or_default();
                let id = this.pending_channel.write().take();
                self.rt.spawn(async move {
                    if let Some(id) = id {
                        let name = name.trim().to_string();
                        if !name.is_empty() {
                            if let Err(e) = this
                                .shell
                                .update_channel(id, serde_json::json!({ "name": name }))
                                .await
                            {
                                this.shell.set_error(e);
                            } else {
                                (this.on_toast)("Canal renombrado".into(), "success".into());
                            }
                        }
                    }
                    let _ = weak.upgrade_in_event_loop(move |ui| ui.set_overlay("none".into()));
                    (this.on_changed)();
                });
            }
            _ => {}
        }
    }

    fn remove_friend(self: &Arc<Self>, user_id: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            if let Err(e) = this.shell.remove_friend(user_id).await {
                this.shell.set_error(e);
            }
            (this.on_changed)();
        });
    }

    fn username_submit(self: &Arc<Self>, username: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            match this.api.update_username(&username).await {
                Ok(user) => {
                    *this.auth.user.write() = Some(user);
                    this.shell.load_servers().await; // refresh author names
                    (this.on_toast)("Nombre de usuario actualizado".into(), "success".into());
                }
                Err(e) => this.shell.set_error(e.code),
            }
            (this.on_changed)();
        });
    }

    fn password_submit(self: &Arc<Self>, current: String, new: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            if let Err(e) = this.shell.change_password(current, new).await {
                this.shell.set_error(e);
            } else {
                (this.on_toast)("Contraseña actualizada".into(), "success".into());
            }
            (this.on_changed)();
        });
    }

    /// Abrir el selector de archivos nativo (rfd) y reflejar la ruta elegida
    /// en la UI. El diálogo bloquea → spawn_blocking; nunca en el UI thread.
    fn avatar_pick(self: &Arc<Self>) {
        let this = self.clone();
        let weak = this.weak();
        self.rt.spawn(async move {
            let picked = tokio::task::spawn_blocking(|| {
                rfd::FileDialog::new()
                    .set_title("Elegir avatar")
                    .add_filter("Imágenes PNG", &["png"])
                    .pick_file()
                    .map(|p| p.to_string_lossy().to_string())
            })
            .await
            .ok()
            .flatten();
            if let Some(path) = picked {
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_avatar_path(path.into()));
            }
        });
    }

    /// Upload an avatar from a local PNG file (Fase 4). Sanitizado:
    /// 1) extensión `.png` (el backend sirve el asset como `image/png` fijo),
    /// 2) firma mágica PNG verificada (no basta la extensión — un .png con
    ///    bytes arbitrarios se rechaza), 3) tamaño ≤ 5 MB (límite del API).
    fn avatar_submit(self: &Arc<Self>, path: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            if !path.to_ascii_lowercase().ends_with(".png") {
                this.shell.set_error("el avatar debe ser un archivo .png".into());
                (this.on_changed)();
                return;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    this.shell.set_error(format!("no se pudo leer el avatar: {e}"));
                    (this.on_changed)();
                    return;
                }
            };
            if bytes.len() > 5 * 1024 * 1024 {
                this.shell.set_error("el avatar debe ser ≤ 5 MB".into());
                (this.on_changed)();
                return;
            }
            const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
            if !bytes.starts_with(PNG_MAGIC) {
                this.shell.set_error("el archivo no es un PNG válido".into());
                (this.on_changed)();
                return;
            }
            match this.api.put_bytes("/api/me/avatar", bytes, "image/png").await {
                Ok(_) => (this.on_toast)("Avatar actualizado".into(), "success".into()),
                Err(e) => this.shell.set_error(e.code),
            }
            (this.on_changed)();
        });
    }

    // -- shared derivations (used by the orchestrator's push_shell) ---------

    pub fn current_server(&self) -> Option<lumen_core::Server> {
        let id = self.shell.selected_server_id.read().clone()?;
        self.shell
            .servers
            .read()
            .iter()
            .find(|s| s.server.id == id)
            .map(|s| s.server.clone())
    }

    pub fn current_channels(&self) -> Vec<lumen_core::Channel> {
        let id = self.shell.selected_server_id.read().clone();
        self.shell
            .servers
            .read()
            .iter()
            .find(|s| Some(&s.server.id) == id.as_ref())
            .map(|s| s.channels.clone())
            .unwrap_or_default()
    }

    pub fn selected_channel_kind(&self) -> Option<ChannelKind> {
        self.shell.selected_channel().map(|c| c.kind)
    }

    pub fn voice_visible(&self) -> bool {
        match self.selected_channel_kind() {
            Some(ChannelKind::Voice) => true,
            Some(ChannelKind::Dm) => *self.shell.dm_call.read(),
            _ => false,
        }
    }

    pub fn chat_title(&self) -> String {
        match self.shell.selected_channel() {
            Some(c) if c.kind == ChannelKind::Dm => self
                .shell
                .dm_list
                .read()
                .iter()
                .find(|d| d.channel.id == c.id)
                .map(|d| d.other_username.clone())
                .unwrap_or_else(|| c.name.clone()),
            Some(c) => c.name,
            None => String::new(),
        }
    }

    // -- actions -----------------------------------------------------------

    fn select_server(self: &Arc<Self>, id: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            this.shell.select_server(id).await;
            (this.on_changed)();
        });
    }

    fn create_server(self: &Arc<Self>, name: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            if let Err(e) = this.shell.create_server(name).await {
                this.shell.set_error(e);
            } else {
                (this.on_toast)("Servidor creado".into(), "success".into());
            }
            (this.on_changed)();
        });
    }

    fn join_server(self: &Arc<Self>, code: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            if let Err(e) = this.shell.join_server(code).await {
                this.shell.set_error(e);
            } else {
                (this.on_toast)("Unido al servidor".into(), "success".into());
            }
            (this.on_changed)();
        });
    }

    fn open_friends(self: &Arc<Self>) {
        *self.shell.view.write() = View::Friends;
        (self.on_changed)();
    }

    fn select_channel(self: &Arc<Self>, id: String) {
        // Presence subscriptions are per-channel (attachment-based): drop the
        // previous subscription, take the new one (Fase 3).
        if let Some(prev) = self.shell.selected_channel_id.read().clone() {
            if prev != id {
                self.shell.unsubscribe_channel(prev);
            }
        }
        self.shell.subscribe_channel(id.clone());
        let this = self.clone();
        self.rt.spawn(async move {
            this.shell.select_channel(id).await;
            // Auto-join: entering a voice channel connects immediately
            // (the dedicated Join button is a second path, not a gate).
            if this.selected_channel_kind() == Some(ChannelKind::Voice) {
                this.join_selected_voice().await;
            }
            (this.on_changed)();
        });
    }

    fn create_channel(self: &Arc<Self>, name: String, kind: String) {
        let kind = if kind == "voice" { ChannelKind::Voice } else { ChannelKind::Text };
        let this = self.clone();
        self.rt.spawn(async move {
            if let Err(e) = this.shell.create_channel(name, kind).await {
                this.shell.set_error(e);
            } else {
                (this.on_toast)("Canal creado".into(), "success".into());
            }
            (this.on_changed)();
        });
    }

    fn copy_invite(self: &Arc<Self>) {
        if let Some(server) = self.current_server() {
            let code = server.invite_code;
            // Clipboard access can block; do it off the UI thread. The
            // arboard Clipboard MUST outlive the call: on X11 the selection
            // is served lazily by the owning process, so dropping the handle
            // right after set_text clears the clipboard (copy "did nothing").
            let _ = std::thread::spawn(move || {
                let mut guard = crate::controller::CLIPBOARD.lock().unwrap();
                if guard.is_none() {
                    match arboard::Clipboard::new() {
                        Ok(cb) => *guard = Some(cb),
                        Err(e) => {
                            eprintln!("copy invite: no clipboard: {e:?}");
                            return;
                        }
                    }
                }
                if let Err(e) = guard.as_mut().unwrap().set_text(code) {
                    eprintln!("copy invite: clipboard write failed: {e:?}");
                }
            });
            (self.on_toast)("Invitación copiada al portapapeles".into(), "success".into());
        }
    }

    fn open_dm(self: &Arc<Self>, username: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            let _ = this.shell.open_dm(username.clone()).await;
            // P2P DM (ADR-006): when the other user is online, open the
            // data-only channel — typing/delivery ride P2P, persistence stays
            // server-side (presence WS chat). Signaling relays via dm-signal.
            let peer = this
                .shell
                .friends
                .read()
                .iter()
                .find(|f| f.user.username == username)
                .map(|f| f.user.id.clone());
            if let Some(peer) = peer {
                if this.shell.presence.online_friends.read().contains_key(&peer) {
                    let ice = match this.api.get_realtime_config().await {
                        Ok(cfg) => cfg.ice_servers.iter().map(|s| lumen_voice::IceServer {
                            urls: match &s.urls {
                                serde_json::Value::String(u) => vec![u.clone()],
                                serde_json::Value::Array(arr) => arr
                                    .iter()
                                    .filter_map(|v| v.as_str().map(|x| x.to_string()))
                                    .collect(),
                                _ => vec![],
                            },
                            username: s.username.clone(),
                            credential: s.credential.clone(),
                        }).collect(),
                        Err(_) => vec![],
                    };
                    let presence = this.shell.presence_client.clone();
                    let peer_relay = peer.clone();
                    this.voice
                        .dm_open(peer.clone(), ice, move |out| {
                            let msg = match out {
                                lumen_voice::dm::DmSignalOut::Offer(sdp) => lumen_core::PresenceOut::DmSignal {
                                    to: peer_relay.clone(),
                                    kind: lumen_core::DmSignalKind::Offer,
                                    sdp: Some(sdp),
                                    candidate: None,
                                },
                                lumen_voice::dm::DmSignalOut::Answer(sdp) => lumen_core::PresenceOut::DmSignal {
                                    to: peer_relay.clone(),
                                    kind: lumen_core::DmSignalKind::Answer,
                                    sdp: Some(sdp),
                                    candidate: None,
                                },
                                lumen_voice::dm::DmSignalOut::Ice(c) => lumen_core::PresenceOut::DmSignal {
                                    to: peer_relay.clone(),
                                    kind: lumen_core::DmSignalKind::Ice,
                                    sdp: None,
                                    candidate: Some(c),
                                },
                            };
                            presence.send(msg);
                        })
                        .await;
                }
            }
            (this.on_changed)();
        });
    }

    fn accept_friend(self: &Arc<Self>, id: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            match this.shell.accept_friend(id).await {
                Ok(_) => (this.on_toast)("Solicitud aceptada".into(), "success".into()),
                Err(e) => this.shell.set_error(e),
            }
            (this.on_changed)();
        });
    }

    fn decline_friend(self: &Arc<Self>, id: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            match this.shell.decline_friend(id).await {
                Ok(_) => (this.on_toast)("Solicitud rechazada".into(), "info".into()),
                Err(e) => this.shell.set_error(e),
            }
            (this.on_changed)();
        });
    }

    fn add_friend(self: &Arc<Self>, username: String) {
        let this = self.clone();
        self.rt.spawn(async move {
            match this.shell.send_friend_request(username).await {
                Ok(_) => (this.on_toast)("Solicitud de amistad enviada".into(), "success".into()),
                Err(e) => this.shell.set_error(e),
            }
            (this.on_changed)();
        });
    }

    /// Join the currently selected voice channel (shared by the Join button
    /// and the auto-join on channel select).
    pub async fn join_selected_voice(&self) {
        let Some(channel) = self.shell.selected_channel() else { return };
        let Some(user) = self.auth.user.read().clone() else { return };
        let Some(token) = self.api.token() else { return };
        let backend = self.auth.backend_url();
        let name = channel.name.clone();
        let channel_id = channel.id.clone();
        let user_id = user.id.clone();
        let username = user.username.clone();
        self.voice.join(backend, token, user_id, username, channel_id, name).await;
    }
}
