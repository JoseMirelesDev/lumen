//! AuthController — login/register/logout/session-bootstrap slice of the
//! AppWindow contract. Owns `is_register` mode, auth-error surfacing and the
//! event-bus listener that triggers bootstrap on Authenticated / reset on
//! LoggedOut. Re-pushes the UI via the shared `on_changed` closure.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use lumen_core::{ApiClient, AuthService, CoreEvent, EventBus, ShellState};
use parking_lot::RwLock;
use slint::{ComponentHandle, Weak};

use crate::sound::{Sfx, SfxEvent};
use crate::voice::VoiceController;
use crate::AppWindow;

pub struct AuthController {
    pub api: Arc<ApiClient>,
    pub auth: Arc<AuthService>,
    pub shell: Arc<ShellState>,
    pub voice: Arc<VoiceController>,
    pub rt: tokio::runtime::Handle,
    bus: EventBus,
    sfx: Arc<Sfx>,
    weak: RwLock<Option<Weak<AppWindow>>>,
    is_register: AtomicBool,
    /// Injected by UiController: re-push the whole UI after a state change.
    on_changed: Arc<dyn Fn() + Send + Sync>,
}

impl AuthController {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        api: Arc<ApiClient>,
        auth: Arc<AuthService>,
        shell: Arc<ShellState>,
        voice: Arc<VoiceController>,
        bus: EventBus,
        rt: tokio::runtime::Handle,
        sfx: Arc<Sfx>,
        on_changed: Arc<dyn Fn() + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            api,
            auth,
            shell,
            voice,
            rt,
            bus,
            sfx,
            weak: RwLock::new(None),
            is_register: AtomicBool::new(false),
            on_changed,
        })
    }

    pub fn attach(self: &Arc<Self>, ui: &AppWindow) {
        *self.weak.write() = Some(ui.as_weak());
        // Initial state push (backend URL, register mode) — the shell data is
        // pushed by the orchestrator's bootstrap path.
        let weak = ui.as_weak();
        let auth = self.auth.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            ui.set_backend_url(auth.backend_url().into());
            ui.set_is_register(false);
        });
        self.wire(ui);
        // Session restore / 401 logout: bootstrap or reset on core events.
        let this = self.clone();
        let mut rx = self.bus.subscribe();
        self.rt.spawn(async move {
            if this.auth.user.read().is_some() {
                this.bootstrap();
            }
            while let Ok(ev) = rx.recv().await {
                match ev {
                    CoreEvent::Authenticated { .. } => this.bootstrap(),
                    CoreEvent::LoggedOut => {
                        this.voice.leave().await;
                        (this.on_changed)();
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
        let weak = this.weak.read().clone().unwrap();
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
        ui.on_login_oauth(move |provider| this.oauth(provider.to_string()));
        let this = self.clone();
        ui.on_logout(move || this.logout());
    }

    /// Open the OAuth flow in the OS browser (Fase 4). The callback redirects
    /// to `lumen://auth/callback?token=…` and the OS re-opens the app; the
    /// deeplink is handled in main.rs (restore_from_deeplink).
    fn oauth(self: &Arc<Self>, provider: String) {
        let url = format!("{}/api/oauth/{}?client=desktop", self.auth.backend_url(), provider);
        #[cfg(not(target_os = "windows"))]
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
        #[cfg(target_os = "windows")]
        let _ = std::process::Command::new("cmd").args(["/c", "start", ""]).arg(&url).spawn();
    }

    fn login(self: &Arc<Self>, username: String, password: String) {
        let this = self.clone();
        let weak = this.weak.read().clone().unwrap();
        let register = this.is_register.load(Ordering::SeqCst);
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
                    (this.on_changed)();
                } else {
                    this.sfx.play(SfxEvent::Error);
                }
            });
        });
    }

    fn logout(self: &Arc<Self>) {
        let this = self.clone();
        let weak = this.weak.read().clone().unwrap();
        self.rt.spawn(async move {
            this.voice.leave().await;
            this.shell.disconnect_presence().await;
            this.auth.logout().await;
            this.shell.reset();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_dm_call(false);
                ui.set_overlay("none".into());
            });
            (this.on_changed)();
        });
    }

    /// After a successful login or session restore: load servers + friends,
    /// then open the presence socket and start the chat retransmitter.
    fn bootstrap(self: &Arc<Self>) {
        let this = self.clone();
        self.rt.spawn(async move {
            this.shell.load_servers().await;
            this.shell.load_friends().await;
            this.shell.connect_presence().await;
            this.shell.spawn_retransmitter(this.rt.clone());
            (this.on_changed)();
        });
    }
}
