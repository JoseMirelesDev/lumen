//! Presence WebSocket client (protocolo presence-v2, protocol/presence-v2.md).
//!
//! One socket per session, opened at login and kept for the session's life:
//! presence (friends/servers online, voice occupancy), real-time chat with
//! ACK, typing, and the DM signaling relay for P2P data channels (ADR-006).
//!
//! The read loop parses `PresenceServerMessage` and publishes typed
//! [`CoreEvent`]s; the UI layer bridges those into the shell state. Reconnect
//! with exponential backoff (1s, 2s, 4s, … max 30s) keeps the socket alive
//! across network drops; `disconnect` stops the loop.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use parking_lot::RwLock;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::event::{CoreEvent, EventBus};
use crate::protocol::*;

/// Outbound messages this client may send on the presence socket.
#[derive(Debug, Clone)]
pub enum PresenceOut {
    Status(PresenceV2Status),
    VoiceJoin { channel_id: String, server_id: String },
    VoiceLeave,
    Chat {
        channel_id: String,
        server_id: String,
        content: String,
        client_id: String,
        reply_to: Option<String>,
        attachment_url: Option<String>,
    },
    ChatEdit { channel_id: String, server_id: String, message_id: String, content: String, client_id: String },
    ChatDelete { channel_id: String, server_id: String, message_id: String, client_id: String },
    Typing { channel_id: String, server_id: String },
    Subscribe(String),
    Unsubscribe(String),
    DmSignal { to: String, kind: DmSignalKind, sdp: Option<String>, candidate: Option<serde_json::Value> },
}

#[derive(Clone)]
pub struct PresenceClient {
    api: Arc<crate::api::ApiClient>,
    bus: EventBus,
    tx: Arc<RwLock<Option<mpsc::UnboundedSender<PresenceOut>>>>,
    task: Arc<RwLock<Option<JoinHandle<()>>>>,
}

impl PresenceClient {
    pub fn new(api: Arc<crate::api::ApiClient>, bus: EventBus) -> Self {
        Self { api, bus, tx: Arc::new(RwLock::new(None)), task: Arc::new(RwLock::new(None)) }
    }

    /// Connect (or reconnect) the presence socket. `servers`/`friends` are the
    /// member-server and accepted-friend ids resolved from the shell state.
    /// Connect (or reconnect) the presence socket. The backend resolves the
    /// user's servers/friends from the JWT (getUserPresenceContext), so the
    /// client only needs its access token.
    pub async fn connect(&self) {
        self.disconnect().await;
        let (tx, mut rx) = mpsc::unbounded_channel::<PresenceOut>();
        *self.tx.write() = Some(tx);

        let api = self.api.clone();
        let bus = self.bus.clone();
        let url = api.base_url();
        let handle = tokio::spawn(async move {
            run_presence_loop(&api, &bus, &url, &mut rx).await;
        });
        *self.task.write() = Some(handle);
    }

    /// Close the socket and stop the loop (logout / teardown).
    pub async fn disconnect(&self) {
        self.tx.write().take();
        let task = self.task.write().take(); // guard dropped before await
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }

    /// Fire a message onto the presence socket; no-op when disconnected.
    pub fn send(&self, msg: PresenceOut) {
        if let Some(tx) = self.tx.read().as_ref() {
            let _ = tx.send(msg);
        }
    }

    pub fn is_connected(&self) -> bool {
        self.tx.read().is_some()
    }
}

async fn run_presence_loop(
    api: &Arc<crate::api::ApiClient>,
    bus: &EventBus,
    base_url: &str,
    out: &mut mpsc::UnboundedReceiver<PresenceOut>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let Some(token) = api.token() else {
            // Logged out — stop reconnecting.
            return;
        };
        let ws_url = format!(
            "{}?token={}",
            base_url.replace("https", "wss").replace("http", "ws").trim_end_matches('/').to_string() + "/api/presence",
            url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>(),
        );
        match tokio_tungstenite::connect_async(&ws_url).await {
            Ok((ws, _)) => {
                backoff = Duration::from_secs(1); // reset on success
                let (mut write, mut read) = ws.split();
                let connected = read_loop(bus, out, &mut write, &mut read).await;
                if !connected {
                    return; // user disconnected / logged out
                }
                // Socket died (network) → backoff and reconnect.
            }
            Err(_) => {
                // Wait before retrying; stop if disconnected meanwhile.
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = out.recv() => {}
                }
                if out.is_closed() {
                    return;
                }
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Drive the socket: forward outbound frames, parse inbound ones. Returns
/// false when the loop should end for good (closed sender / logged out).
async fn read_loop<S, R>(
    bus: &EventBus,
    out: &mut mpsc::UnboundedReceiver<PresenceOut>,
    write: &mut S,
    read: &mut R,
) -> bool
where
    S: SinkExt<Message> + Unpin,
    R: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        tokio::select! {
            maybe = out.recv() => {
                let Some(msg) = maybe else { return false };
                if let Some(frame) = encode_out(msg) {
                    if write.send(frame).await.is_err() {
                        return true; // socket broke → reconnect
                    }
                }
            }
            frame = read.next() => {
                let Some(frame) = frame else { return true };
                let Ok(frame) = frame else { return true };
                let Message::Text(text) = frame else { continue };
                let Ok(server_msg) = serde_json::from_str::<PresenceServerMessage>(&text) else { continue };
                publish_event(bus, server_msg);
            }
        }
    }
}

fn encode_out(msg: PresenceOut) -> Option<Message> {
    let client_msg: PresenceClientMessage = match msg {
        PresenceOut::Status(s) => PresenceClientMessage::Status { status: s },
        PresenceOut::VoiceJoin { channel_id, server_id } => PresenceClientMessage::VoiceJoin { channel_id, server_id },
        PresenceOut::VoiceLeave => PresenceClientMessage::VoiceLeave,
        PresenceOut::Chat { channel_id, server_id, content, client_id, reply_to, attachment_url } => PresenceClientMessage::Chat { channel_id, server_id, content, client_id, reply_to, attachment_url },
        PresenceOut::ChatEdit { channel_id, server_id, message_id, content, client_id } => PresenceClientMessage::ChatEdit { channel_id, server_id, message_id, content, client_id },
        PresenceOut::ChatDelete { channel_id, server_id, message_id, client_id } => PresenceClientMessage::ChatDelete { channel_id, server_id, message_id, client_id },
        PresenceOut::Typing { channel_id, server_id } => PresenceClientMessage::Typing { channel_id, server_id },
        PresenceOut::Subscribe(c) => PresenceClientMessage::Subscribe { channel_id: c },
        PresenceOut::Unsubscribe(c) => PresenceClientMessage::Unsubscribe { channel_id: c },
        PresenceOut::DmSignal { to, kind, sdp, candidate } => PresenceClientMessage::DmSignal { to, kind, sdp, candidate },
    };
    match serde_json::to_string(&client_msg) {
        Ok(json) => Some(Message::Text(json.into())),
        Err(_) => None,
    }
}

fn publish_event(bus: &EventBus, msg: PresenceServerMessage) {
    let ev = match msg {
        PresenceServerMessage::Ready { online_friends, servers } => CoreEvent::PresenceReady { online_friends, servers },
        PresenceServerMessage::FriendOnline { user_id, username } => CoreEvent::FriendOnline { user_id, username },
        PresenceServerMessage::FriendOffline { user_id } => CoreEvent::FriendOffline { user_id },
        PresenceServerMessage::FriendStatus { user_id, status } => CoreEvent::FriendStatus { user_id, status },
        PresenceServerMessage::VoiceUpdate { server_id, channel_id, peers } => CoreEvent::VoiceOccupancyChanged { server_id, channel_id, peers },
        PresenceServerMessage::MemberOnline { server_id, user_id, username } => CoreEvent::MemberOnline { server_id, user_id, username },
        PresenceServerMessage::MemberOffline { server_id, user_id } => CoreEvent::MemberOffline { server_id, user_id },
        PresenceServerMessage::Typing { channel_id, user_id } => CoreEvent::Typing { channel_id, user_id },
        PresenceServerMessage::SubscribeAck { channel_id } => CoreEvent::SubscribeAck { channel_id },
        PresenceServerMessage::Chat { channel_id, message } => CoreEvent::RealtimeMessage { channel_id, message },
        PresenceServerMessage::ChatAck { client_id, message_id, created_at } => CoreEvent::ChatAck { client_id, message_id, created_at },
        PresenceServerMessage::ChatEditAck { client_id, message_id } => CoreEvent::ChatEditAck { client_id, message_id },
        PresenceServerMessage::ChatDeleteAck { client_id, message_id } => CoreEvent::ChatDeleteAck { client_id, message_id },
        PresenceServerMessage::ChatEdited { channel_id, message } => CoreEvent::ChatEdited { channel_id, message },
        PresenceServerMessage::ChatDeleted { channel_id, message_id } => CoreEvent::ChatDeleted { channel_id, message_id },
        PresenceServerMessage::ChatError { client_id, code } => CoreEvent::ChatError { client_id, code },
        PresenceServerMessage::Reaction { channel_id, message_id, emoji, user_id, added } => CoreEvent::Reaction { channel_id, message_id, emoji, user_id, added },
        PresenceServerMessage::DmOffer { from, sdp } => CoreEvent::DmOffer { from, sdp },
        PresenceServerMessage::DmAnswer { from, sdp } => CoreEvent::DmAnswer { from, sdp },
        PresenceServerMessage::DmIce { from, candidate } => CoreEvent::DmIce { from, candidate },
        PresenceServerMessage::Pong => return,
        PresenceServerMessage::Error { code, message } => CoreEvent::Error { message: format!("presence: {code}: {message}") },
    };
    bus.publish(ev);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_wire_format() {
        let m = PresenceOut::Chat {
            channel_id: "ch-1".into(),
            server_id: "s-1".into(),
            content: "hola".into(),
            client_id: "c-1".into(),
            reply_to: None,
            attachment_url: None,
        };
        let frame = encode_out(m).unwrap();
        let Message::Text(t) = frame else { panic!("not text") };
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&t).unwrap(),
            serde_json::json!({"type": "chat", "channelId": "ch-1", "serverId": "s-1", "content": "hola", "clientId": "c-1"})
        );
    }

    #[test]
    fn inbound_ready_parses() {
        let json = serde_json::json!({
            "type": "ready",
            "onlineFriends": [{"userId": "f1", "username": "bob", "status": "online"}],
            "servers": [{"serverId": "s1", "onlineMembers": [{"userId": "u1", "username": "alice"}], "voiceChannels": [{"channelId": "ch1", "peers": [{"userId": "u2", "username": "bob"}]}]}]
        });
        let m: PresenceServerMessage = serde_json::from_value(json).unwrap();
        match m {
            PresenceServerMessage::Ready { online_friends, servers } => {
                assert_eq!(online_friends[0].user_id, "f1");
                assert_eq!(servers[0].voice_channels[0].peers[0].user_id, "u2");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn inbound_chat_ack_parses() {
        let m: PresenceServerMessage = serde_json::from_value(serde_json::json!({
            "type": "chat-ack", "clientId": "c1", "messageId": "m1", "createdAt": "2026-08-13T10:00:00.000Z"
        })).unwrap();
        match m {
            PresenceServerMessage::ChatAck { client_id, message_id, .. } => {
                assert_eq!(client_id, "c1");
                assert_eq!(message_id, "m1");
            }
            _ => panic!("wrong variant"),
        }
    }
}
