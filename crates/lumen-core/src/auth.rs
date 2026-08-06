//! AuthService: session state (token/user/backendUrl) + login/register/logout.
//! Port of `apps/desktop/src/lib/stores/auth.svelte.ts`. Token persists via
//! [`Settings`]; the API client's base + token are updated here.

use std::sync::Arc;

use parking_lot::RwLock;

use crate::api::{ApiClient, ApiError};
use crate::event::EventBus;
use crate::protocol::User;
use crate::settings::Settings;

pub struct AuthService {
    pub api: Arc<ApiClient>,
    settings: Arc<Settings>,
    pub user: RwLock<Option<User>>,
    /// True while an auth request is in flight.
    pub busy: RwLock<bool>,
    pub error: RwLock<Option<String>>,
    bus: EventBus,
}

impl AuthService {
    pub fn new(api: Arc<ApiClient>, settings: Arc<Settings>, bus: EventBus) -> Arc<Self> {
        let svc = Arc::new(Self {
            api,
            settings,
            user: RwLock::new(None),
            busy: RwLock::new(false),
            error: RwLock::new(None),
            bus,
        });
        // Restore persisted session: token + backend URL.
        let backend = svc.settings.backend_url();
        svc.api.set_base(backend);
        if let Some(token) = svc.settings.token() {
            svc.api.set_token(Some(token));
            let svc2 = svc.clone();
            let _ = std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("tokio rt");
                rt.block_on(svc2.refresh_me());
            });
        }
        svc
    }

    pub fn backend_url(&self) -> String {
        self.settings.backend_url()
    }

    pub fn set_backend_url(&self, url: String) {
        let url = url.trim().trim_end_matches('/').to_string();
        self.settings.set_backend_url(url.clone());
        self.api.set_base(url);
        *self.error.write() = None;
    }

    pub fn set_error(&self, msg: Option<String>) {
        *self.error.write() = msg;
    }

    pub async fn refresh_me(&self) {
        match self.api.me().await {
            Ok(resp) => {
                *self.user.write() = Some(resp.user.clone());
                self.bus.publish(crate::event::CoreEvent::Authenticated { user: resp.user });
            }
            Err(ApiError { status, .. }) if status == 401 => {
                // Session expired; drop it.
                self.clear_session();
            }
            Err(e) => {
                *self.error.write() = Some(e.code);
                self.clear_session();
            }
        }
    }

    /// Returns true on success (mirrors the Svelte store's `authenticate`).
    pub async fn login(&self, username: &str, password: &str) -> bool {
        self.authenticate(self.api.login(username, password)).await
    }

    pub async fn register(&self, username: &str, password: &str) -> bool {
        self.authenticate(self.api.register(username, password)).await
    }

    async fn authenticate(&self, call: impl std::future::Future<Output = Result<crate::protocol::AuthResponse, ApiError>>) -> bool {
        *self.busy.write() = true;
        *self.error.write() = None;
        let result = call.await;
        *self.busy.write() = false;
        match result {
            Ok(resp) => {
                self.api.set_token(Some(resp.token.clone()));
                self.settings.set_token(Some(resp.token));
                *self.user.write() = Some(resp.user.clone());
                self.bus.publish(crate::event::CoreEvent::Authenticated { user: resp.user });
                true
            }
            Err(e) => {
                let code = if e.status == 0 { "network_error".to_string() } else { e.code.clone() };
                *self.error.write() = Some(code);
                false
            }
        }
    }

    pub fn logout(&self) {
        self.clear_session();
    }

    pub fn clear_session(&self) {
        *self.user.write() = None;
        self.api.set_token(None);
        self.settings.set_token(None);
        self.bus.publish(crate::event::CoreEvent::LoggedOut);
    }
}
