//! Signaling client for the channel Durable Object (docs/protocol.md §1).
//!
//! WebSocket JSON frames; the token rides as `?token=` on the upgrade URL (the
//! browser/WebView WebSocket cannot set headers). Messages mirror
//! `@lumen/protocol` `ClientMessage` / `ServerMessage`.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

/// Peer identity as relayed by the DO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    #[serde(rename = "peerId")]
    pub peer_id: String,
    #[serde(rename = "userId")]
    pub user_id: String,
    /// Display name, sent by the peer on join and relayed by the DO.
    #[serde(default)]
    pub username: String,
}

/// Inbound messages from the DO, re-typed for the session.
#[derive(Debug, Clone)]
pub enum SignalEvent {
    Joined { peer_id: String, peers: Vec<PeerInfo> },
    PeerJoined(PeerInfo),
    PeerLeft(String),
    /// `from` is the sender's peerId.
    Offer { from: String, sdp: String },
    Answer { from: String, sdp: String },
    IceCandidate { from: String, candidate: Value },
    Error { code: String, message: String },
    /// The socket closed; the session is over. `replaced` = the server
    /// evicted this connection because the same user re-joined elsewhere —
    /// the host must NOT auto-reconnect, it would fight the new connection.
    Closed { replaced: bool },
}

/// Outbound messages this client may send.
#[derive(Debug, Clone)]
pub enum SignalOut {
    Join { channel_id: String, user_id: String, username: String },
    Offer { to: String, sdp: String },
    Answer { to: String, sdp: String },
    IceCandidate { to: String, candidate: Value },
    /// Heartbeat: the DO answers `pong`; also keeps the DO awake so it
    /// processes peer disconnects promptly.
    Ping,
    /// Close the socket now: the writer stops forwarding and closes the WS.
    /// The session sends this on leave — dropping the sender alone is not
    /// enough, because the heartbeat task holds a clone of the outbound
    /// channel and keeps the writer (and thus the socket) alive forever.
    Close,
}

// -- Wire format -------------------------------------------------------------

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum ClientMessage {
    #[serde(rename_all = "camelCase")]
    Join { channel_id: String, user_id: String, username: String },
    Offer { to: String, sdp: String },
    Answer { to: String, sdp: String },
    IceCandidate { to: String, candidate: Value },
    Ping,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum ServerMessage {
    #[serde(rename_all = "camelCase")]
    Joined { peer_id: String, peers: Vec<PeerInfo> },
    #[serde(rename = "peer-joined")]
    PeerJoined { peer: PeerInfo },
    /// Field is `peerId` on the wire, like `joined` — without the rename the
    /// DO's `{"type":"peer-left","peerId":...}` fails to parse and the event
    /// is silently dropped, leaving a ghost peer tile in the UI forever.
    #[serde(rename = "peer-left", rename_all = "camelCase")]
    PeerLeft { peer_id: String },
    Offer { from: String, sdp: String },
    Answer { from: String, sdp: String },
    IceCandidate { from: String, candidate: Value },
    Presence { user_id: String, status: String },
    Pong,
    Error { code: String, message: String },
}

// -- Client ------------------------------------------------------------------

pub struct SignalingClient {
    /// Send outbound messages from any task.
    pub tx: mpsc::UnboundedSender<SignalOut>,
}

impl SignalingClient {
    /// Connect, send `join`, and spawn the reader/writer loop. Returns once the
    /// socket is open and `join` is queued (not necessarily accepted), plus the
    /// inbound event stream.
    pub async fn connect(
        ws_url: &str,
        token: &str,
        channel_id: &str,
        user_id: &str,
        username: &str,
    ) -> anyhow::Result<(Self, mpsc::UnboundedReceiver<SignalEvent>)> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let url = format!(
            "{}?token={}",
            ws_url,
            url::form_urlencoded::byte_serialize(token.as_bytes()).collect::<String>()
        );
        let (ws, _) = tokio_tungstenite::connect_async(&url).await?;
        let (mut write, mut read) = ws.split();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalOut>();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<SignalEvent>();
        let (alive_tx, mut alive_rx) = mpsc::unbounded_channel::<()>();

        // Reader task: emit events; any inbound frame means the socket lives.
        let alive_tx_reader = alive_tx.clone();
        let ev_tx_reader = ev_tx.clone();
        let ev_tx_hb = ev_tx.clone();
        let reader = {
            async move {
                let mut replaced = false;
                while let Some(msg) = read.next().await {
                    let msg = match msg {
                        Ok(msg) => msg,
                        Err(_) => break,
                    };
                    let text = match msg {
                        Message::Text(t) => t,
                        Message::Close(Some(frame)) => {
                            // The DO evicts stale connections for the same
                            // user with close 4000 "replaced".
                            use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
                            replaced = frame.code == CloseCode::Bad(4000u16);
                            break;
                        }
                        Message::Close(None) => break,
                        _ => continue,
                    };
                    let _ = alive_tx_reader.send(());
                    let parsed = match serde_json::from_str::<ServerMessage>(&text) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    let event = match parsed {
                        ServerMessage::Joined { peer_id, peers } => SignalEvent::Joined { peer_id, peers },
                        ServerMessage::PeerJoined { peer } => SignalEvent::PeerJoined(peer),
                        ServerMessage::PeerLeft { peer_id } => SignalEvent::PeerLeft(peer_id),
                        ServerMessage::Offer { from, sdp } => SignalEvent::Offer { from, sdp },
                        ServerMessage::Answer { from, sdp } => SignalEvent::Answer { from, sdp },
                        ServerMessage::IceCandidate { from, candidate } => {
                            SignalEvent::IceCandidate { from, candidate }
                        }
                        ServerMessage::Error { code, message } => SignalEvent::Error { code, message },
                        _ => continue,
                    };
                    if ev_tx_reader.send(event).is_err() {
                        break;
                    }
                }
                let _ = ev_tx.send(SignalEvent::Closed { replaced });
            }
        };
        // Spawn the reader FIRST: the writer aborts it on Close so both halves
        // of the split stream drop, which is what actually closes the TCP
        // connection — a close frame alone is not enough, the reader half
        // keeps the stream (and the socket) alive.
        let reader_handle = tokio::spawn(reader);

        // Writer task: serialize + send, flush on drop.
        tokio::spawn(async move {
                while let Some(msg) = out_rx.recv().await {
                    let m = match &msg {
                        SignalOut::Join { channel_id, user_id, username } => ClientMessage::Join {
                            channel_id: channel_id.clone(),
                            user_id: user_id.clone(),
                            username: username.clone(),
                        },
                        SignalOut::Offer { to, sdp } => ClientMessage::Offer { to: to.clone(), sdp: sdp.clone() },
                        SignalOut::Answer { to, sdp } => ClientMessage::Answer { to: to.clone(), sdp: sdp.clone() },
                        SignalOut::IceCandidate { to, candidate } => {
                            ClientMessage::IceCandidate { to: to.clone(), candidate: candidate.clone() }
                        }
                        SignalOut::Ping => ClientMessage::Ping,
                        SignalOut::Close => {
                            // Session over: abort the reader half so both
                            // halves of the split stream drop. That tears the
                            // TCP connection down; the DO sees it and
                            // broadcasts peer-left.
                            reader_handle.abort();
                            break;
                        }
                    };
                    let text = match serde_json::to_string(&m) {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    if write.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                let _ = write.flush().await;
                let _ = write.close().await;
        });

        // Heartbeat: ping the DO every 5 s and declare the socket dead if
        // nothing comes back for 20 s. Both timers use tokio's monotonic
        // clock, which does NOT advance across system suspend — a laptop that
        // hibernates for hours wakes up with the deadline still fresh, sends
        // its next ping, and resumes normally instead of spuriously dropping
        // the session.
        let out_hb = out_tx.clone();
        tokio::spawn(async move {
            let mut ping = tokio::time::interval(Duration::from_secs(5));
            let mut deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            loop {
                tokio::select! {
                    _ = ping.tick() => {
                        let _ = out_hb.send(SignalOut::Ping);
                        deadline = tokio::time::Instant::now() + Duration::from_secs(20);
                    }
                    alive = alive_rx.recv() => {
                        match alive {
                            Some(()) => deadline = tokio::time::Instant::now() + Duration::from_secs(20),
                            None => break, // reader ended → socket closed
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        let _ = ev_tx_hb.send(SignalEvent::Closed { replaced: false });
                        break;
                    }
                }
            }
        });

        out_tx.send(SignalOut::Join {
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
            username: username.to_string(),
        })?;

        Ok((Self { tx: out_tx }, ev_rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_wire_format() {
        let m = ClientMessage::Join { channel_id: "ch-1".into(), user_id: "u-1".into(), username: "alice".into() };
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            serde_json::json!({"type": "join", "channelId": "ch-1", "userId": "u-1", "username": "alice"})
        );
        let m = ClientMessage::Offer { to: "p-2".into(), sdp: "v=0".into() };
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            serde_json::json!({"type": "offer", "to": "p-2", "sdp": "v=0"})
        );
        let m = ClientMessage::Ping;
        assert_eq!(serde_json::to_value(&m).unwrap(), serde_json::json!({"type": "ping"}));
    }

    #[test]
    fn server_message_wire_format() {
        let json = serde_json::json!({
            "type": "joined",
            "peerId": "me",
            "peers": [{"peerId": "p", "userId": "u"}]
        });
        let m: ServerMessage = serde_json::from_value(json).unwrap();
        match m {
            ServerMessage::Joined { peer_id, peers } => {
                assert_eq!(peer_id, "me");
                assert_eq!(peers[0].peer_id, "p");
            }
            _ => panic!("wrong variant"),
        }
        let m: ServerMessage = serde_json::from_value(serde_json::json!({"type": "peer-left", "peerId": "p"})).unwrap();
        match m {
            ServerMessage::PeerLeft { peer_id } => assert_eq!(peer_id, "p"),
            _ => panic!("wrong variant"),
        }
        let m: ServerMessage = serde_json::from_value(serde_json::json!({"type": "ice-candidate", "from": "x", "candidate": {"candidate": "candidate:1 1 UDP 1 1.2.3.4 123 typ host"}})).unwrap();
        match m {
            ServerMessage::IceCandidate { from, candidate } => {
                assert_eq!(from, "x");
                assert_eq!(candidate["candidate"], "candidate:1 1 UDP 1 1.2.3.4 123 typ host");
            }
            _ => panic!("wrong variant"),
        }
    }
}
