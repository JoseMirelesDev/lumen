//! Typed REST client for the Lumen API (docs/protocol.md §4). Auth is injected
//! per-request via the shared token slot so logout/session expiry is handled in
//! one place by the caller. Port of `apps/desktop/src/lib/api.ts`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::de::DeserializeOwned;

use crate::protocol::*;

/// API error carrying the HTTP status + machine-readable code from the worker.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{code} (http {status})")]
pub struct ApiError {
    pub status: u16,
    pub code: String,
}

impl ApiError {
    pub fn code(&self) -> &str {
        &self.code
    }
}

/// 401 recovery hook (Fase 1, ADR-0007): the AuthService registers a
/// refresh-then-retry closure. Returns true when the access token was
/// refreshed. Registered after construction (AuthService owns ApiClient, not
/// the other way around — no construction cycle).
pub type RefreshHook = dyn Fn() -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync;

#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: Arc<RwLock<String>>,
    token: Arc<RwLock<Option<String>>>,
    refresh_hook: Arc<RwLock<Option<Arc<RefreshHook>>>>,
}

impl ApiClient {
    pub fn new(base: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .build()
            .expect("build reqwest client");
        Self {
            http,
            base: Arc::new(RwLock::new(base.into())),
            token: Arc::new(RwLock::new(None)),
            refresh_hook: Arc::new(RwLock::new(None)),
        }
    }

    pub fn set_base(&self, base: String) {
        *self.base.write() = base.trim_end_matches('/').to_string();
    }

    pub fn set_token(&self, token: Option<String>) {
        *self.token.write() = token;
    }

    pub fn token(&self) -> Option<String> {
        self.token.read().clone()
    }

    /// Register the 401→refresh→retry hook (called by AuthService).
    pub fn set_refresh_hook(&self, hook: Option<Arc<RefreshHook>>) {
        *self.refresh_hook.write() = hook;
    }

    fn base(&self) -> String {
        self.base.read().clone()
    }

    /// Current backend base URL (used to build the presence WS endpoint).
    pub fn base_url(&self) -> String {
        self.base()
    }

    /// One HTTP round-trip with auth + JSON, no retry logic. Used by the
    /// refresh path itself (never re-enters `request`).
    async fn raw_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<reqwest::Response, ApiError> {
        let url = format!("{}{}", self.base(), path);
        let mut req = self.http.request(method, &url);
        if let Some(token) = self.token() {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(b) = body {
            req = req.header(reqwest::header::CONTENT_TYPE, "application/json").json(b);
        }
        req.send().await.map_err(|e| ApiError { status: 0, code: format!("network_error: {e}") })
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, ApiError> {
        let mut res = self.raw_request(method.clone(), path, body).await?;
        // 401 → refresh the access token once, then retry (ADR-0007). A
        // failed refresh publishes LoggedOut via the AuthService hook. The
        // read guard must drop before the await (parking_lot guards are !Send).
        if res.status().as_u16() == 401 {
            let hook = self.refresh_hook.read().clone();
            let refreshed = match hook {
                Some(hook) => hook().await,
                None => false,
            };
            if refreshed {
                res = self.raw_request(method, path, body).await?;
            }
        }
        let status = res.status().as_u16();
        let data: Option<serde_json::Value> = res.json().await.ok();
        if !(200..300).contains(&status) {
            let code = data
                .as_ref()
                .and_then(|d| d.get("error").and_then(|e| e.as_str()))
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("http_{status}"));
            return Err(ApiError { status, code });
        }
        serde_json::from_value(data.unwrap_or(serde_json::Value::Null))
            .map_err(|e| ApiError { status, code: format!("deserialize: {e}") })
    }

    pub async fn register(&self, username: &str, password: &str) -> Result<AuthResponse, ApiError> {
        let body = serde_json::json!({ "username": username, "password": password });
        self.request(reqwest::Method::POST, "/api/auth/register", Some(&body)).await
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<AuthResponse, ApiError> {
        let body = serde_json::json!({ "username": username, "password": password });
        self.request(reqwest::Method::POST, "/api/auth/login", Some(&body)).await
    }

    /// Rotate the refresh token (ADR-0007): the presented token is revoked
    /// server-side; the response carries the fresh access + refresh pair.
    pub async fn refresh(&self, refresh_token: &str) -> Result<RefreshResponse, ApiError> {
        let body = serde_json::json!({ "refreshToken": refresh_token });
        self.request(reqwest::Method::POST, "/api/auth/refresh", Some(&body)).await
    }

    /// Revoke the refresh token server-side (logout).
    pub async fn logout(&self, refresh_token: &str) -> Result<serde_json::Value, ApiError> {
        let body = serde_json::json!({ "refreshToken": refresh_token });
        self.request(reqwest::Method::POST, "/api/auth/logout", Some(&body)).await
    }

    pub async fn me(&self) -> Result<MeResponse, ApiError> {
        self.request(reqwest::Method::GET, "/api/me", None).await
    }

    pub async fn create_server(&self, name: &str) -> Result<CreateServerResponse, ApiError> {
        let body = serde_json::json!({ "name": name });
        self.request(reqwest::Method::POST, "/api/servers", Some(&body)).await
    }

    pub async fn list_servers(&self) -> Result<Vec<ServerWithChannels>, ApiError> {
        self.request(reqwest::Method::GET, "/api/servers", None).await
    }

    pub async fn join_server(&self, invite_code: &str) -> Result<CreateServerResponse, ApiError> {
        let body = serde_json::json!({ "inviteCode": invite_code });
        self.request(reqwest::Method::POST, "/api/servers/join", Some(&body)).await
    }

    pub async fn get_server(&self, server_id: &str) -> Result<ServerDetail, ApiError> {
        let path = format!("/api/servers/{}", encode(server_id));
        self.request(reqwest::Method::GET, &path, None).await
    }

    pub async fn create_channel(
        &self,
        server_id: &str,
        name: &str,
        kind: ChannelKind,
    ) -> Result<CreateChannelResponse, ApiError> {
        let body = serde_json::json!({ "name": name, "kind": kind.as_str() });
        let path = format!("/api/servers/{}/channels", encode(server_id));
        self.request(reqwest::Method::POST, &path, Some(&body)).await
    }

    pub async fn post_message(&self, channel_id: &str, content: &str) -> Result<MessageResponse, ApiError> {
        let body = serde_json::json!({ "content": content });
        let path = format!("/api/channels/{}/messages", encode(channel_id));
        self.request(reqwest::Method::POST, &path, Some(&body)).await
    }

    pub async fn list_messages(&self, channel_id: &str, limit: u32) -> Result<Vec<TextMessage>, ApiError> {
        let path = format!("/api/channels/{}/messages?limit={}", encode(channel_id), limit);
        self.request(reqwest::Method::GET, &path, None).await
    }

    pub async fn get_friends(&self) -> Result<FriendsResponse, ApiError> {
        self.request(reqwest::Method::GET, "/api/friends", None).await
    }

    pub async fn send_friend_request(&self, username: &str) -> Result<RequestResponse, ApiError> {
        let body = serde_json::json!({ "username": username });
        self.request(reqwest::Method::POST, "/api/friends/requests", Some(&body)).await
    }

    pub async fn accept_friend_request(&self, request_id: &str) -> Result<FriendAcceptedResponse, ApiError> {
        let path = format!("/api/friends/requests/{}/accept", encode(request_id));
        self.request(reqwest::Method::POST, &path, None).await
    }

    pub async fn decline_friend_request(&self, request_id: &str) -> Result<serde_json::Value, ApiError> {
        let path = format!("/api/friends/requests/{}", encode(request_id));
        self.request(reqwest::Method::DELETE, &path, None).await
    }

    pub async fn list_dms(&self) -> Result<Vec<DmSummary>, ApiError> {
        self.request(reqwest::Method::GET, "/api/dms", None).await
    }

    pub async fn create_dm(&self, username: &str) -> Result<DmCreatedResponse, ApiError> {
        let body = serde_json::json!({ "username": username });
        self.request(reqwest::Method::POST, "/api/dms", Some(&body)).await
    }

    pub async fn get_realtime_config(&self) -> Result<RealtimeConfig, ApiError> {
        self.request(reqwest::Method::GET, "/api/realtime/config", None).await
    }

    // ------------------------------------------------------------------
    // Fase 2 — CRUD
    // ------------------------------------------------------------------

    pub async fn update_server(&self, id: &str, name: Option<&str>) -> Result<ServerDetail, ApiError> {
        let path = format!("/api/servers/{}", encode(id));
        let body = serde_json::json!({ "name": name });
        self.request(reqwest::Method::PATCH, &path, Some(&body)).await
    }

    pub async fn delete_server(&self, id: &str) -> Result<(), ApiError> {
        let path = format!("/api/servers/{}?confirm=true", encode(id));
        self.request::<serde_json::Value>(reqwest::Method::DELETE, &path, None).await?;
        Ok(())
    }

    pub async fn leave_server(&self, id: &str) -> Result<(), ApiError> {
        let path = format!("/api/servers/{}/leave", encode(id));
        self.request::<serde_json::Value>(reqwest::Method::POST, &path, None).await?;
        Ok(())
    }

    pub async fn regenerate_invite(&self, id: &str) -> Result<String, ApiError> {
        let path = format!("/api/servers/{}/invite", encode(id));
        let res: serde_json::Value =
            self.request(reqwest::Method::POST, &path, None).await?;
        Ok(res.get("inviteCode").and_then(|v| v.as_str()).unwrap_or_default().to_string())
    }

    pub async fn kick_member(&self, server_id: &str, user_id: &str) -> Result<(), ApiError> {
        let path = format!("/api/servers/{}/members/{}", encode(server_id), encode(user_id));
        self.request::<serde_json::Value>(reqwest::Method::DELETE, &path, None).await?;
        Ok(())
    }

    pub async fn update_channel(
        &self,
        id: &str,
        patch: &serde_json::Value,
    ) -> Result<Channel, ApiError> {
        let path = format!("/api/channels/{}", encode(id));
        let res: serde_json::Value =
            self.request(reqwest::Method::PATCH, &path, Some(patch)).await?;
        serde_json::from_value(res.get("channel").cloned().unwrap_or(serde_json::Value::Null))
            .map_err(|e| ApiError { status: 0, code: format!("deserialize: {e}") })
    }

    pub async fn delete_channel(&self, id: &str) -> Result<(), ApiError> {
        let path = format!("/api/channels/{}", encode(id));
        self.request::<serde_json::Value>(reqwest::Method::DELETE, &path, None).await?;
        Ok(())
    }

    pub async fn edit_message(&self, id: &str, content: &str) -> Result<TextMessage, ApiError> {
        let path = format!("/api/messages/{}", encode(id));
        let body = serde_json::json!({ "content": content });
        let res: serde_json::Value = self.request(reqwest::Method::PATCH, &path, Some(&body)).await?;
        serde_json::from_value(res.get("message").cloned().unwrap_or(serde_json::Value::Null))
            .map_err(|e| ApiError { status: 0, code: format!("deserialize: {e}") })
    }

    pub async fn delete_message(&self, id: &str) -> Result<(), ApiError> {
        let path = format!("/api/messages/{}", encode(id));
        self.request::<serde_json::Value>(reqwest::Method::DELETE, &path, None).await?;
        Ok(())
    }

    pub async fn update_username(&self, username: &str) -> Result<User, ApiError> {
        let body = serde_json::json!({ "username": username });
        let res: serde_json::Value = self.request(reqwest::Method::PATCH, "/api/me", Some(&body)).await?;
        serde_json::from_value(res.get("user").cloned().unwrap_or(serde_json::Value::Null))
            .map_err(|e| ApiError { status: 0, code: format!("deserialize: {e}") })
    }

    pub async fn change_password(&self, current: &str, new: &str) -> Result<(), ApiError> {
        let body = serde_json::json!({ "currentPassword": current, "newPassword": new });
        self.request::<serde_json::Value>(reqwest::Method::PUT, "/api/me/password", Some(&body)).await?;
        Ok(())
    }

    pub async fn delete_account(&self) -> Result<(), ApiError> {
        self.request::<serde_json::Value>(reqwest::Method::DELETE, "/api/me", None).await?;
        Ok(())
    }

    pub async fn remove_friend(&self, user_id: &str) -> Result<(), ApiError> {
        let path = format!("/api/friends/{}", encode(user_id));
        self.request::<serde_json::Value>(reqwest::Method::DELETE, &path, None).await?;
        Ok(())
    }

    pub async fn delete_dm(&self, channel_id: &str) -> Result<(), ApiError> {
        let path = format!("/api/dms/{}", encode(channel_id));
        self.request::<serde_json::Value>(reqwest::Method::DELETE, &path, None).await?;
        Ok(())
    }

    pub async fn ban_member(&self, server_id: &str, user_id: &str, reason: Option<&str>) -> Result<(), ApiError> {
        let path = format!("/api/servers/{}/bans", encode(server_id));
        let body = serde_json::json!({ "userId": user_id, "reason": reason });
        self.request::<serde_json::Value>(reqwest::Method::POST, &path, Some(&body)).await?;
        Ok(())
    }

    pub async fn unban_member(&self, server_id: &str, user_id: &str) -> Result<(), ApiError> {
        let path = format!("/api/servers/{}/bans/{}", encode(server_id), encode(user_id));
        self.request::<serde_json::Value>(reqwest::Method::DELETE, &path, None).await?;
        Ok(())
    }

    pub async fn list_bans(&self, server_id: &str) -> Result<serde_json::Value, ApiError> {
        let path = format!("/api/servers/{}/bans", encode(server_id));
        self.request(reqwest::Method::GET, &path, None).await
    }

    pub async fn block_user(&self, user_id: &str) -> Result<(), ApiError> {
        let body = serde_json::json!({ "userId": user_id });
        self.request::<serde_json::Value>(reqwest::Method::POST, "/api/blocks", Some(&body)).await?;
        Ok(())
    }

    pub async fn unblock_user(&self, user_id: &str) -> Result<(), ApiError> {
        let path = format!("/api/blocks/{}", encode(user_id));
        self.request::<serde_json::Value>(reqwest::Method::DELETE, &path, None).await?;
        Ok(())
    }

    pub async fn report(&self, target_type: &str, target_id: &str, reason: Option<&str>) -> Result<(), ApiError> {
        let body = serde_json::json!({ "targetType": target_type, "targetId": target_id, "reason": reason });
        self.request::<serde_json::Value>(reqwest::Method::POST, "/api/reports", Some(&body)).await?;
        Ok(())
    }

    pub async fn transfer_server(&self, server_id: &str, user_id: &str) -> Result<(), ApiError> {
        let path = format!("/api/servers/{}/transfer", encode(server_id));
        let body = serde_json::json!({ "userId": user_id });
        self.request::<serde_json::Value>(reqwest::Method::POST, &path, Some(&body)).await?;
        Ok(())
    }

    /// Toggle a reaction on a message (Fase 6.1).
    pub async fn toggle_reaction(&self, message_id: &str, emoji: &str) -> Result<(), ApiError> {
        let path = format!("/api/messages/{}/reactions/{}", encode(message_id), encode(emoji));
        self.request::<serde_json::Value>(reqwest::Method::PUT, &path, None).await?;
        Ok(())
    }

    /// Aggregated reaction counts for message ids (Fase 6.1).
    pub async fn get_reactions(&self, channel_id: &str, message_ids: &[&str]) -> Result<serde_json::Value, ApiError> {
        let ids = message_ids.join(",");
        let path = format!("/api/channels/{}/messages/reactions?messageIds={}", encode(channel_id), encode(&ids));
        self.request(reqwest::Method::GET, &path, None).await
    }

    /// Upload an attachment → R2 url (Fase 6.4).
    pub async fn upload_attachment(&self, filename: &str, bytes: Vec<u8>, content_type: &str) -> Result<String, ApiError> {
        let path = format!("/api/uploads?filename={}", encode(filename));
        let res = self.put_bytes(&path, bytes, content_type).await?;
        Ok(res.get("url").and_then(|u| u.as_str()).unwrap_or_default().to_string())
    }

    /// Raw-body PUT (avatar / server icon uploads, Fase 4).
    pub async fn put_bytes(&self, path: &str, bytes: Vec<u8>, content_type: &str) -> Result<serde_json::Value, ApiError> {
        let url = format!("{}{}", self.base(), path);
        let mut req = self.http.request(reqwest::Method::PUT, &url);
        if let Some(token) = self.token() {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        req = req
            .header(reqwest::header::CONTENT_TYPE, content_type)
            // Uploads (avatar/íconos, hasta 5 MB) necesitan más margen que el
            // default de 30s del cliente HTTP.
            .timeout(std::time::Duration::from_secs(60))
            .body(bytes);
        let res = req.send().await.map_err(|e| ApiError { status: 0, code: format!("network_error: {e}") })?;
        let status = res.status().as_u16();
        let data: Option<serde_json::Value> = res.json().await.ok();
        if !(200..300).contains(&status) {
            let code = data
                .as_ref()
                .and_then(|d| d.get("error").and_then(|e| e.as_str()))
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("http_{status}"));
            return Err(ApiError { status, code });
        }
        Ok(data.unwrap_or(serde_json::Value::Null))
    }
}

fn encode(id: &str) -> String {
    url::form_urlencoded::byte_serialize(id.as_bytes()).collect()
}
