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
    /// R2 key (migration 0003/0004).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
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
    /// Migration 0003.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Manual ordering within the server (migration 0003).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
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
    /// Set when edited (migration 0003).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<String>,
    /// Set when soft-deleted — clients render a placeholder (migration 0003).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    /// Id of the replied-to message (migration 0003, Fase 6.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

/// Message block row (ADR-0004): 1 row = up to 50 packed messages.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageBlock {
    pub id: String,
    pub channel_id: String,
    pub count: u32,
    pub first_at: String,
    pub last_at: String,
}

/// PATCH /api/messages/:id response (Fase 2, provisional per ADR-0010).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditMessageResult {
    pub id: String,
    pub content: String,
    pub edited_at: String,
}

/// Minimal permission model (Fase 2/5): owner vs member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerRole {
    Owner,
    Member,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthResponse {
    pub token: String,
    /// Present on register/login (Fase 1, ADR-0007). Optional for
    /// compatibility with responses that predate refresh tokens.
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub user: User,
}

/// POST /api/auth/refresh — rotation response (ADR-0007).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshResponse {
    pub token: String,
    pub refresh_token: String,
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

// ---------------------------------------------------------------------------
// Presence v2 (WS /api/presence → PresenceHubDO) — protocol/presence-v2.md.
// Mirror of @lumen/protocol presence types. The v1 PresenceStatus above
// stays for the ChannelDO voice broadcasts; the hub uses PresenceV2Status.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PresenceV2Status {
    Online,
    Idle,
    Dnd,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerLite {
    pub user_id: String,
    pub username: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnlineFriendLite {
    pub user_id: String,
    pub username: String,
    pub status: PresenceV2Status,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceChannelPresence {
    pub channel_id: String,
    pub peers: Vec<PeerLite>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerPresence {
    pub server_id: String,
    pub online_members: Vec<PeerLite>,
    pub voice_channels: Vec<VoiceChannelPresence>,
}

/// One buffered chat message (ADR-0004): packed N-per-row in message_blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BufferedMessage {
    pub id: String,
    pub author_id: String,
    pub author_name: String,
    pub content: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DmSignalKind {
    Offer,
    Answer,
    Ice,
}

/// Messages the client sends to the PresenceHubDO.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PresenceClientMessage {
    Ready,
    #[serde(rename_all = "camelCase")]
    Status { status: PresenceV2Status },
    #[serde(rename_all = "camelCase")]
    VoiceJoin { channel_id: String, server_id: String },
    VoiceLeave,
    #[serde(rename_all = "camelCase")]
    Chat {
        channel_id: String,
        server_id: String,
        content: String,
        client_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        attachment_url: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    ChatEdit { channel_id: String, server_id: String, message_id: String, content: String, client_id: String },
    #[serde(rename_all = "camelCase")]
    ChatDelete { channel_id: String, server_id: String, message_id: String, client_id: String },
    #[serde(rename_all = "camelCase")]
    Typing { channel_id: String, server_id: String },
    #[serde(rename_all = "camelCase")]
    Subscribe { channel_id: String },
    #[serde(rename_all = "camelCase")]
    Unsubscribe { channel_id: String },
    #[serde(rename_all = "camelCase")]
    DmSignal { to: String, kind: DmSignalKind, #[serde(skip_serializing_if = "Option::is_none")] sdp: Option<String>, #[serde(skip_serializing_if = "Option::is_none")] candidate: Option<serde_json::Value> },
    #[serde(rename_all = "camelCase")]
    ReactionToggle { channel_id: String, server_id: String, message_id: String, emoji: String },
    Ping,
}

/// Messages the PresenceHubDO sends to clients.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PresenceServerMessage {
    #[serde(rename_all = "camelCase")]
    Ready { online_friends: Vec<OnlineFriendLite>, servers: Vec<ServerPresence> },
    #[serde(rename_all = "camelCase")]
    FriendOnline { user_id: String, username: String },
    #[serde(rename_all = "camelCase")]
    FriendOffline { user_id: String },
    #[serde(rename_all = "camelCase")]
    FriendStatus { user_id: String, status: PresenceV2Status },
    #[serde(rename_all = "camelCase")]
    VoiceUpdate { server_id: String, channel_id: String, peers: Vec<PeerLite> },
    #[serde(rename_all = "camelCase")]
    MemberOnline { server_id: String, user_id: String, username: String },
    #[serde(rename_all = "camelCase")]
    MemberOffline { server_id: String, user_id: String },
    #[serde(rename_all = "camelCase")]
    Typing { channel_id: String, user_id: String },
    #[serde(rename_all = "camelCase")]
    SubscribeAck { channel_id: String },
    #[serde(rename_all = "camelCase")]
    Chat { channel_id: String, message: BufferedMessage },
    #[serde(rename_all = "camelCase")]
    ChatAck { client_id: String, message_id: String, created_at: String },
    #[serde(rename_all = "camelCase")]
    ChatEditAck { client_id: String, message_id: String },
    #[serde(rename_all = "camelCase")]
    ChatDeleteAck { client_id: String, message_id: String },
    #[serde(rename_all = "camelCase")]
    ChatEdited { channel_id: String, message: EditedMessage },
    #[serde(rename_all = "camelCase")]
    ChatDeleted { channel_id: String, message_id: String },
    #[serde(rename_all = "camelCase")]
    ChatError { client_id: String, code: String },
    #[serde(rename_all = "camelCase")]
    Reaction { channel_id: String, message_id: String, emoji: String, user_id: String, added: bool },
    #[serde(rename_all = "camelCase")]
    DmOffer { from: String, sdp: String },
    #[serde(rename_all = "camelCase")]
    DmAnswer { from: String, sdp: String },
    #[serde(rename_all = "camelCase")]
    DmIce { from: String, candidate: serde_json::Value },
    Pong,
    #[serde(rename_all = "camelCase")]
    Error { code: String, message: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditedMessage {
    pub id: String,
    pub content: String,
    pub edited_at: String,
}
