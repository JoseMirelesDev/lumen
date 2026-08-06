//! Typed REST client for the Lumen API (docs/protocol.md §4). Auth is injected
//! per-request via the shared token slot so logout/session expiry is handled in
//! one place by the caller. Port of `apps/desktop/src/lib/api.ts`.

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

#[derive(Clone)]
pub struct ApiClient {
    http: reqwest::Client,
    base: Arc<RwLock<String>>,
    token: Arc<RwLock<Option<String>>>,
}

impl ApiClient {
    pub fn new(base: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .build()
            .expect("build reqwest client");
        Self { http, base: Arc::new(RwLock::new(base.into())), token: Arc::new(RwLock::new(None)) }
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

    fn base(&self) -> String {
        self.base.read().clone()
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, ApiError> {
        let url = format!("{}{}", self.base(), path);
        let mut req = self.http.request(method, &url);
        if let Some(token) = self.token() {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(b) = body {
            req = req.header(reqwest::header::CONTENT_TYPE, "application/json").json(b);
        }
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
}

fn encode(id: &str) -> String {
    url::form_urlencoded::byte_serialize(id.as_bytes()).collect()
}
