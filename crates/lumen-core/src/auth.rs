//! AuthService: session state (token/refresh/user/backendUrl) + login/register/
//! logout. Port of `apps/desktop/src/lib/stores/auth.svelte.ts`. The access
//! token persists in memory only (1h TTL, ADR-0007); the opaque refresh token
//! persists via [`Settings`] and is rotated on every 401 recovery.
//!
//! The 401→refresh→retry hook is registered on the [`ApiClient`] here
//! (AuthService owns the bus, so a failed refresh publishes `LoggedOut`).

use std::future::Future;
use std::pin::Pin;
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
    refresh_token: RwLock<Option<String>>,
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
            refresh_token: RwLock::new(None),
            bus,
        });
        // 401 recovery hook: refresh the access token, retry once. A failed
        // refresh (revoked/expired/network) clears the session — the caller
        // sees LoggedOut and returns to the login screen.
        let weak = Arc::downgrade(&svc);
        svc.api.set_refresh_hook(Some(Arc::new(move || {
            let weak = weak.clone();
            Box::pin(async move {
                match weak.upgrade() {
                    Some(svc) => svc.try_refresh().await,
                    None => false,
                }
            }) as Pin<Box<dyn Future<Output = bool> + Send>>
        })));
        // Restore persisted session: backend URL + refresh token + access
        // token (if any), then re-validate via /api/me.
        let backend = svc.settings.backend_url();
        svc.api.set_base(backend);
        *svc.refresh_token.write() = svc.settings.refresh_token();
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
                // Session expired; the refresh hook already tried and failed,
                // so the session is cleared.
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
                *self.refresh_token.write() = resp.refresh_token.clone();
                self.settings.set_refresh_token(resp.refresh_token);
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

    /// Rotate the refresh token and update both tokens. False when there is
    /// no refresh token or the rotation failed — the session is cleared
    /// (publishes LoggedOut) so the UI returns to the login screen.
    pub async fn try_refresh(&self) -> bool {
        let rt = self.refresh_token.read().clone();
        let Some(rt) = rt else { return false };
        match self.api.refresh(&rt).await {
            Ok(res) => {
                self.api.set_token(Some(res.token.clone()));
                self.settings.set_token(Some(res.token));
                *self.refresh_token.write() = Some(res.refresh_token.clone());
                self.settings.set_refresh_token(Some(res.refresh_token));
                true
            }
            Err(_) => {
                self.clear_session();
                false
            }
        }
    }

    /// Server-side revoke of the refresh token, then local session teardown.
    pub async fn logout(&self) {
        // Bind the clone first: the read guard is !Send and must drop before
        // the await.
        let rt = self.refresh_token.read().clone();
        if let Some(rt) = rt {
            let _ = self.api.logout(&rt).await;
        }
        self.clear_session();
    }

    /// Local teardown only (no server call): used on session expiry and
    /// after a server-side revoke.
    pub fn clear_session(&self) {
        *self.user.write() = None;
        self.api.set_token(None);
        *self.refresh_token.write() = None;
        self.settings.set_token(None);
        self.settings.set_refresh_token(None);
        self.bus.publish(crate::event::CoreEvent::LoggedOut);
    }

    /// OAuth deeplink (Fase 4): `lumen://auth/callback?token=…&refreshToken=…`.
    /// Stores both tokens and validates the session via /api/me.
    pub fn restore_from_deeplink(self: &Arc<Self>, token: String, refresh_token: Option<String>) {
        self.api.set_token(Some(token.clone()));
        self.settings.set_token(Some(token));
        if let Some(rt) = refresh_token {
            *self.refresh_token.write() = Some(rt.clone());
            self.settings.set_refresh_token(Some(rt));
        }
        let svc = self.clone();
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio rt");
            rt.block_on(svc.refresh_me());
        });
    }
}
