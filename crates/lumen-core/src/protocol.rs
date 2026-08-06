//! Rust mirror of `@lumen/protocol` (packages/protocol/src/index.ts) — the
//! single source of truth for the wire contract. Fields are camelCase on the
//! wire; keep these types in lockstep with the TS definitions.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// WebSocket signaling (client <-> LumenChannelDO)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresenceStatus {
    Online,
    Idle,
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub peer_id: String,
    pub user_id: String,
}

/// Messages the client sends to the channel Durable Object.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientMessage {
    #[serde(rename_all = "camelCase")]
    Join { channel_id: String, user_id: String },
    #[serde(rename_all = "camelCase")]
    Offer { to: String, sdp: String },
    #[serde(rename_all = "camelCase")]
    Answer { to: String, sdp: String },
    #[serde(rename_all = "camelCase")]
    IceCandidate { to: String, candidate: serde_json::Value },
    #[serde(rename_all = "camelCase")]
    Presence { status: PresenceStatus },
    Ping,
}

/// Messages the channel Durable Object sends to clients.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ServerMessage {
    #[serde(rename_all = "camelCase")]
    Joined { peer_id: String, peers: Vec<PeerInfo> },
    #[serde(rename_all = "camelCase")]
    PeerJoined { peer: PeerInfo },
    #[serde(rename_all = "camelCase")]
    PeerLeft { peer_id: String },
    #[serde(rename_all = "camelCase")]
    Offer { from: String, sdp: String },
    #[serde(rename_all = "camelCase")]
    Answer { from: String, sdp: String },
    #[serde(rename_all = "camelCase")]
    IceCandidate { from: String, candidate: serde_json::Value },
    #[serde(rename_all = "camelCase")]
    Presence { user_id: String, status: PresenceStatus },
    Pong,
    #[serde(rename_all = "camelCase")]
    Error { code: String, message: String },
}

// ---------------------------------------------------------------------------
// REST entities
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    pub username: String,
    pub last_seen: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    pub id: String,
    pub name: String,
    pub owner_id: String,
    pub invite_code: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Text,
    Voice,
    Dm,
}

impl ChannelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelKind::Text => "text",
            ChannelKind::Voice => "voice",
            ChannelKind::Dm => "dm",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    pub id: String,
    /// Empty for DM channels.
    pub server_id: String,
    pub name: String,
    pub kind: ChannelKind,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerWithChannels {
    pub server: Server,
    pub channels: Vec<Channel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendshipRequest {
    pub id: String,
    pub user: User,
    /// "incoming" = someone asked me; "outgoing" = I asked them.
    pub direction: RequestDirection,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FriendInfo {
    pub user: User,
    /// shared servers count, for display.
    pub shared_servers: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DmSummary {
    pub channel: Channel,
    /// the other participant's username.
    pub other_username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextMessage {
    pub id: String,
    pub channel_id: String,
    pub author_id: String,
    pub author_name: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthResponse {
    pub token: String,
    pub user: User,
}

/// Shape compatible with RTCIceServer (the host casts).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IceServerConfig {
    pub urls: serde_json::Value, // string | string[] on the wire
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeConfig {
    pub ice_servers: Vec<IceServerConfig>,
}

/// GET /api/servers/:id — members list.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerDetail {
    pub server: Server,
    pub channels: Vec<Channel>,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub id: String,
    pub username: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MeResponse {
    pub user: User,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateServerResponse {
    pub server: Server,
    pub channels: Vec<Channel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateChannelResponse {
    pub channel: Channel,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageResponse {
    pub message: TextMessage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FriendsResponse {
    pub friends: Vec<FriendInfo>,
    pub pending: Vec<FriendshipRequest>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RequestResponse {
    pub request: FriendshipRequest,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FriendAcceptedResponse {
    pub friend: User,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DmCreatedResponse {
    pub channel: DmSummary,
}
