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
