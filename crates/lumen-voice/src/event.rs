//! Framework-agnostic voice events: the contract between the voice core and
//! any host (Tauri today, Slint next). Mirrors 1:1 the legacy `voice://` IPC
//! channels so adapters can re-emit byte-identical payloads.

/// What a peer's RTCPeerConnection is doing (surfaced to the UI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Closed,
    Unknown,
}

impl PeerState {
    pub fn as_str(self) -> &'static str {
        match self {
            PeerState::New => "new",
            PeerState::Connecting => "connecting",
            PeerState::Connected => "connected",
            PeerState::Disconnected => "disconnected",
            PeerState::Failed => "failed",
            PeerState::Closed => "closed",
            PeerState::Unknown => "unknown",
        }
    }
}

/// Session-level signaling states surfaced to the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalingState {
    Closed,
    /// The server evicted this connection because the same user re-joined
    /// elsewhere. Do not auto-reconnect — it would fight the new connection.
    Replaced,
}

impl SignalingState {
    pub fn as_str(self) -> &'static str {
        match self {
            SignalingState::Closed => "closed",
            SignalingState::Replaced => "replaced",
        }
    }
}

/// A peer's RMS level (0..1) for the level meter, emitted ~10 Hz.
#[derive(Debug, Clone)]
pub struct PeerLevel {
    pub peer_id: String,
    pub level: f32,
}

/// Events the voice core emits to the host, mirroring the legacy `voice://`
/// IPC channels 1:1.
#[derive(Debug, Clone)]
pub enum VoiceEvent {
    /// `voice://levels` — ~10 Hz metering while a session lives.
    Levels { local: f32, peers: Vec<PeerLevel> },
    /// `voice://debug` — signaling/negotiation diagnostics (optional peer id).
    Debug { peer_id: Option<String>, message: String },
    /// `voice://peer-joined` — a participant joined the channel.
    PeerJoined { peer_id: String, user_id: String, username: String },
    /// `voice://peer-left`.
    PeerLeft { peer_id: String },
    /// `voice://state` — a peer's RTCPeerConnection state changed.
    State { peer_id: String, state: PeerState },
    /// `voice://signaling` — session-level signaling state (closed on teardown).
    Signaling { state: SignalingState },
    /// `voice://error` — `code` present only when the signaling layer reported it.
    Error { code: Option<String>, message: String },
    /// A message from a peer's "chat" data channel (Fase 3, ADR-006): bytes
    /// are JSON text — `{ "type": "typing" | "chat", "content": "..." }`.
    DataChannelMessage { peer_id: String, data: Vec<u8> },
}
