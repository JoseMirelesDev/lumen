//! Signaling client for the channel Durable Object (docs/protocol.md §1).
//!
//! WebSocket JSON frames; the token rides as `?token=` on the upgrade URL (the
//! browser/WebView WebSocket cannot set headers). Messages mirror
//! `@lumen/protocol` `ClientMessage` / `ServerMessage`.

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
    /// The socket closed; the session is over.
    Closed,
}

/// Outbound messages this client may send.
#[derive(Debug, Clone)]
pub enum SignalOut {
    Join { channel_id: String, user_id: String },
    Offer { to: String, sdp: String },
    Answer { to: String, sdp: String },
    IceCandidate { to: String, candidate: Value },
}

// -- Wire format -------------------------------------------------------------

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum ClientMessage {
    #[serde(rename_all = "camelCase")]
    Join { channel_id: String, user_id: String },
    Offer { to: String, sdp: String },
    Answer { to: String, sdp: String },
    IceCandidate { to: String, candidate: Value },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum ServerMessage {
    #[serde(rename_all = "camelCase")]
    Joined { peer_id: String, peers: Vec<PeerInfo> },
    #[serde(rename = "peer-joined")]
    PeerJoined { peer: PeerInfo },
    #[serde(rename = "peer-left")]
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

        // Writer task: serialize + send, flush on drop.
        tokio::spawn(async move {
                while let Some(msg) = out_rx.recv().await {
                    let m = match &msg {
                        SignalOut::Join { channel_id, user_id } => ClientMessage::Join {
                            channel_id: channel_id.clone(),
                            user_id: user_id.clone(),
                        },
                        SignalOut::Offer { to, sdp } => ClientMessage::Offer { to: to.clone(), sdp: sdp.clone() },
                        SignalOut::Answer { to, sdp } => ClientMessage::Answer { to: to.clone(), sdp: sdp.clone() },
                        SignalOut::IceCandidate { to, candidate } => {
                            ClientMessage::IceCandidate { to: to.clone(), candidate: candidate.clone() }
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

        // Reader task.
        let reader = {
            async move {
                while let Some(msg) = read.next().await {
                    let msg = match msg {
                        Ok(msg) => msg,
                        Err(_) => break,
                    };
                    let text = match msg {
                        Message::Text(t) => t,
                        _ => continue,
                    };
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
                    if ev_tx.send(event).is_err() {
                        break;
                    }
                }
                let _ = ev_tx.send(SignalEvent::Closed);
            }
        };
        tokio::spawn(reader);

        out_tx.send(SignalOut::Join {
            channel_id: channel_id.to_string(),
            user_id: user_id.to_string(),
        })?;

        Ok((Self { tx: out_tx }, ev_rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_message_wire_format() {
        let m = ClientMessage::Join { channel_id: "ch-1".into(), user_id: "u-1".into() };
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            serde_json::json!({"type": "join", "channelId": "ch-1", "userId": "u-1"})
        );
        let m = ClientMessage::Offer { to: "p-2".into(), sdp: "v=0".into() };
        assert_eq!(
            serde_json::to_value(&m).unwrap(),
            serde_json::json!({"type": "offer", "to": "p-2", "sdp": "v=0"})
        );
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