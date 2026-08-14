//! Lightweight event bus: everything observable about the app flows as a typed
//! [`CoreEvent`] over a tokio broadcast channel. Consumers (Slint adapter,
//! debug log, plugins in Fase 5) subscribe; producers publish.

use tokio::sync::broadcast;

use crate::protocol::{DmSummary, FriendInfo, FriendshipRequest, ServerWithChannels, TextMessage, User};

#[derive(Debug, Clone)]
pub enum CoreEvent {
    Authenticated { user: User },
    LoggedOut,
    ServersLoaded { servers: Vec<ServerWithChannels> },
    MessagesLoaded { channel_id: String, messages: Vec<TextMessage> },
    FriendsLoaded {
        friends: Vec<FriendInfo>,
        pending: Vec<FriendshipRequest>,
        dms: Vec<DmSummary>,
    },
    /// A recoverable error surfaced to the UI (never panics).
    Error { message: String },
    // Fase 2 — CRUD mutations (mirror the REST responses).
    ServerUpdated { server: crate::protocol::Server },
    ServerDeleted { server_id: String },
    ServerLeft { server_id: String },
    ChannelUpdated { channel: crate::protocol::Channel },
    ChannelDeleted { channel_id: String },
    MessageUpdated { message: TextMessage },
    MessageDeleted { channel_id: String, message_id: String },
    FriendsChanged,
    // Fase 3 — presence + real-time chat (protocol/presence-v2.md).
    PresenceReady {
        online_friends: Vec<crate::protocol::OnlineFriendLite>,
        servers: Vec<crate::protocol::ServerPresence>,
    },
    FriendOnline { user_id: String, username: String },
    FriendOffline { user_id: String },
    FriendStatus { user_id: String, status: crate::protocol::PresenceV2Status },
    VoiceOccupancyChanged {
        server_id: String,
        channel_id: String,
        peers: Vec<crate::protocol::PeerLite>,
    },
    MemberOnline { server_id: String, user_id: String, username: String },
    MemberOffline { server_id: String, user_id: String },
    Typing { channel_id: String, user_id: String },
    /// The DO confirmed the channel subscription — only now may the client
    /// rely on receiving `chat` broadcasts for that channel (ordering across
    /// sockets is not guaranteed; the ack makes subscribe deterministic).
    SubscribeAck { channel_id: String },
    RealtimeMessage { channel_id: String, message: crate::protocol::BufferedMessage },
    ChatAck { client_id: String, message_id: String, created_at: String },
    ChatEditAck { client_id: String, message_id: String },
    ChatDeleteAck { client_id: String, message_id: String },
    ChatEdited { channel_id: String, message: crate::protocol::EditedMessage },
    ChatDeleted { channel_id: String, message_id: String },
    ChatError { client_id: String, code: String },
    /// DM signaling relay (ADR-006): route to the voice client.
    DmOffer { from: String, sdp: String },
    DmAnswer { from: String, sdp: String },
    DmIce { from: String, candidate: serde_json::Value },
    /// A P2P DM data-channel frame (ADR-006): JSON text, `{type, content}`.
    InCallChat { peer_id: String, data: Vec<u8> },
    /// Reaction toggle broadcast (Fase 6.1).
    Reaction { channel_id: String, message_id: String, emoji: String, user_id: String, added: bool },
    /// Transient UI notification (toast): kind = "info" | "success" | "error".
    Toast { message: String, kind: String },
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<CoreEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(128);
        Self { tx }
    }

    pub fn publish(&self, event: CoreEvent) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.tx.subscribe()
    }
}
