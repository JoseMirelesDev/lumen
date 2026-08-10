//! UiController: the glue between the Slint AppWindow and lumen-core.
//! Wires every AppWindow callback to a core action and pushes core state back
//! into Slint models/properties via `Weak<AppWindow>::upgrade_in_event_loop`.
//! Port of the Svelte stores (auth/shell) + their call sites in +page.svelte.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use lumen_core::{ApiClient, AuthService, ChannelKind, CoreEvent, EventBus, ShellState, View};
use parking_lot::RwLock;
use slint::{ComponentHandle, Weak};

use crate::model;
use crate::voice::VoiceController;
use crate::AppWindow;

pub struct UiController {
    pub api: Arc<ApiClient>,
    pub auth: Arc<AuthService>,
    pub shell: Arc<ShellState>,
    pub voice: Arc<VoiceController>,
    bus: EventBus,
    pub rt: tokio::runtime::Handle,
    weak: RwLock<Option<Weak<AppWindow>>>,
    is_register: AtomicBool,
}

impl UiController {
    pub fn new(
        api: Arc<ApiClient>,
        auth: Arc<AuthService>,
        shell: Arc<ShellState>,
        voice: Arc<VoiceController>,
        bus: EventBus,
        rt: tokio::runtime::Handle,
    ) -> Arc<Self> {
        Arc::new(Self {
            api,
            auth,
            shell,
            voice,
            bus,
            rt,
            weak: RwLock::new(None),
            is_register: AtomicBool::new(false),
        })
    }

    fn weak(&self) -> Weak<AppWindow> {
        self.weak.read().clone().expect("UiController not attached")
    }

    pub fn attach(self: &Arc<Self>, ui: &AppWindow) {
        let weak = ui.as_weak();
        *self.weak.write() = Some(weak.clone());
        self.voice.attach(weak.clone());
        let this = self.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_backend_url(this.auth.backend_url().into());
            ui.set_is_register(false);
            this.push_shell(&ui);
        });
        self.wire(ui);
        // React to core events (session restore, 401 logout, ...). The
        // one-shot check in the initial push above races with the background
        // refresh_me() restoring a persisted token, so bootstrap here too:
        // either the initial state or a queued Authenticated event fires it.
        let mut rx = self.bus.subscribe();
        let this = self.clone();
        self.rt.spawn(async move {
            if this.auth.user.read().is_some() {
                this.bootstrap();
            }
            while let Ok(ev) = rx.recv().await {
                match ev {
                    CoreEvent::Authenticated { .. } => this.bootstrap(),
                    CoreEvent::LoggedOut => {
                        this.voice.leave().await;
                        this.push();
                    }
                    _ => {}
                }
            }
        });
    }

    fn wire(self: &Arc<Self>, ui: &AppWindow) {
        let this = self.clone();
        ui.on_login_submit(move |username, password| {
            this.login(username.to_string(), password.to_string());
        });
        let this = self.clone();
        let weak = this.weak();
        ui.on_login_toggle_mode(move || {
            let reg = !this.is_register.fetch_xor(true, Ordering::SeqCst);
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_is_register(reg);
                ui.set_auth_error("".into());
            });
        });
        let this = self.clone();
        ui.on_backend_changed(move |url| {
            this.auth.set_backend_url(url.to_string());
        });
        let this = self.clone();
        ui.on_logout(move || this.logout());
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
        ui.on_create_channel(move |name, kind| this.create_channel(name.to_string(), kind.to_string()));
        let this = self.clone();
        ui.on_copy_invite(move || this.copy_invite());
        let this = self.clone();
        ui.on_send_message(move |content| this.send_message(content.to_string()));
        let this = self.clone();
        ui.on_toggle_dm_call(move || this.toggle_dm_call());
        let this = self.clone();
        ui.on_open_dm(move |username| this.open_dm(username.to_string()));
        let this = self.clone();
        ui.on_accept_friend(move |id| this.accept_friend(id.to_string()));
        let this = self.clone();
        ui.on_decline_friend(move |id| this.decline_friend(id.to_string()));
        let this = self.clone();
        ui.on_add_friend(move |username| this.add_friend(username.to_string()));
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
    }

    // -- helpers -----------------------------------------------------------

    fn current_server(&self) -> Option<lumen_core::Server> {
        let id = self.shell.selected_server_id.read().clone()?;
        self.shell
            .servers
            .read()
            .iter()
            .find(|s| s.server.id == id)
            .map(|s| s.server.clone())
    }

    fn current_channels(&self) -> Vec<lumen_core::Channel> {
        let id = self.shell.selected_server_id.read().clone();
        self.shell
            .servers
            .read()
            .iter()
            .find(|s| Some(&s.server.id) == id.as_ref())
            .map(|s| s.channels.clone())
            .unwrap_or_default()
    }

    fn selected_channel_kind(&self) -> Option<ChannelKind> {
        self.shell.selected_channel().map(|c| c.kind)
    }

    fn voice_visible(&self) -> bool {
        match self.selected_channel_kind() {
            Some(ChannelKind::Voice) => true,
            Some(ChannelKind::Dm) => *self.shell.dm_call.read(),
            _ => false,
        }
    }

    fn chat_title(&self) -> String {
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

    fn login(self: &Arc<Self>, username: String, password: String) {
        let this = self.clone();
        let weak = self.weak();
        let register = self.is_register.load(Ordering::SeqCst);
        self.rt.spawn(async move {
            let ok = if register {
                this.auth.register(&username, &password).await
            } else {
                this.auth.login(&username, &password).await
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_auth_error(this.auth.error.read().clone().unwrap_or_default().into());
                if ok {
                    // Switch to the shell immediately; server/friend data is
                    // loaded by the CoreEvent::Authenticated listener.
                    this.push();
                }
            });
        });
    }

    fn logout(self: &Arc<Self>) {
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            this.voice.leave().await;
            this.auth.logout();
            this.shell.reset();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_dm_call(false);
                this.push_shell(&ui);
            });
        });
    }

    /// After a successful login or session restore: load servers + friends.
    fn bootstrap(self: &Arc<Self>) {
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            this.shell.load_servers().await;
            this.shell.load_friends().await;
            let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
        });
    }

    fn select_server(self: &Arc<Self>, id: String) {
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            this.shell.select_server(id).await;
            let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
        });
    }

    fn create_server(self: &Arc<Self>, name: String) {
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            if let Err(e) = this.shell.create_server(name).await {
                this.shell.set_error(e);
            }
            let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
        });
    }

    fn join_server(self: &Arc<Self>, code: String) {
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            if let Err(e) = this.shell.join_server(code).await {
                this.shell.set_error(e);
            }
            let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
        });
    }

    fn open_friends(self: &Arc<Self>) {
        *self.shell.view.write() = View::Friends;
        self.push();
    }

    fn select_channel(self: &Arc<Self>, id: String) {
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            this.shell.select_channel(id).await;
            let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
        });
    }

    fn create_channel(self: &Arc<Self>, name: String, kind: String) {
        let kind = if kind == "voice" { ChannelKind::Voice } else { ChannelKind::Text };
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            if let Err(e) = this.shell.create_channel(name, kind).await {
                this.shell.set_error(e);
            }
            let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
        });
    }

    fn copy_invite(self: &Arc<Self>) {
        if let Some(server) = self.current_server() {
            let code = server.invite_code;
            // Clipboard access can block; do it off the UI thread. Slint has no
            // clipboard API, so use arboard (X11/Wayland/macOS/Windows).
            let _ = std::thread::spawn(move || {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(code);
                }
            });
        }
    }

    fn send_message(self: &Arc<Self>, content: String) {
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            let _ = this.shell.send_message(content).await;
            let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
        });
    }

    fn toggle_dm_call(self: &Arc<Self>) {
        let on = !*self.shell.dm_call.read();
        *self.shell.dm_call.write() = on;
        self.push();
    }

    fn open_dm(self: &Arc<Self>, username: String) {
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            let _ = this.shell.open_dm(username).await;
            let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
        });
    }

    fn accept_friend(self: &Arc<Self>, id: String) {
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            let _ = this.shell.accept_friend(id).await;
            let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
        });
    }

    fn decline_friend(self: &Arc<Self>, id: String) {
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            let _ = this.shell.decline_friend(id).await;
            let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
        });
    }

    fn add_friend(self: &Arc<Self>, username: String) {
        let this = self.clone();
        let weak = self.weak();
        self.rt.spawn(async move {
            let _ = this.shell.send_friend_request(username).await;
            let _ = weak.upgrade_in_event_loop(move |ui| this.push_shell(&ui));
        });
    }

    fn voice_join(self: &Arc<Self>) {
        let Some(channel) = self.shell.selected_channel() else { return };
        let Some(user) = self.auth.user.read().clone() else { return };
        let Some(token) = self.api.token() else { return };
        let backend = self.auth.backend_url();
        let name = channel.name.clone();
        let channel_id = channel.id.clone();
        let user_id = user.id.clone();
        let username = user.username.clone();
        let this = self.clone();
        self.rt.spawn(async move {
            this.voice.join(backend, token, user_id, username, channel_id, name).await;
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
        let channels = self.current_channels();
        let messages = shell.messages.read().clone();
        let friends = shell.friends.read().clone();
        let pending = shell.pending.read().clone();
        let dm_list = shell.dm_list.read().clone();
        let view = *shell.view.read();
        let dm_call = *shell.dm_call.read();
        let shell_error = shell.error.read().clone().unwrap_or_default();

        ui.set_logged_in(user.is_some());
        ui.set_current_user(model::first_char_upper(&username));
        ui.set_servers(model::servers_model(&servers, &selected_server_id));
        ui.set_channels(model::channels_model(&channels, &selected_channel_id));
        ui.set_channel_list_title(self.current_server().map(|s| s.name).unwrap_or_default().into());
        ui.set_can_create_channel(
            self.current_server().map(|s| s.owner_id == user_id).unwrap_or(false),
        );
        ui.set_messages(model::messages_model(&messages, &user_id));
        ui.set_chat_title(self.chat_title().into());
        ui.set_is_dm(self.selected_channel_kind() == Some(ChannelKind::Dm));
        ui.set_dm_call(dm_call);
        ui.set_friends_view(view == View::Friends);
        ui.set_friends_online(model::friends_online_model(&friends));
        ui.set_friends_offline(model::friends_offline_model(&friends));
        ui.set_friend_requests(model::requests_model(&pending));
        ui.set_dms(model::dms_model(&dm_list));
        ui.set_chat_error(shell_error.clone().into());
        ui.set_friends_error(shell_error.clone().into());
        ui.set_voice_visible(self.voice_visible());
        ui.set_voice_local_user(username.clone().into());
        ui.set_voice_local_initial(model::first_char_upper(&username));
        ui.set_voice_suppressor_model(self.voice.current_suppressor_model().as_str().into());
        // Non-silent fallback: tell the UI whether this CPU can run the
        // FastEnhancer-M engine, so a degraded-to-NS selection is surfaced.
        ui.set_voice_suppressor_model_available(lumen_voice::audio::FastEnhancerDenoiser::available());
        ui.set_voice_aec_enabled(self.voice.current_aec_enabled());
    }
}
