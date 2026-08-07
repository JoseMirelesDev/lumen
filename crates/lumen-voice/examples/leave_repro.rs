//! TEMP DIAG (bug reproduction): does leaving a voice channel actually close
//! the signaling WebSocket?
//!
//! Two SignalingClients on the same channel: an observer and a peer. The peer
//! sends `SignalOut::Close` and drops its sender — exactly what
//! `VoiceClient::leave()` does to the signaling layer. If the WS truly
//! closes, the DO broadcasts `peer-left` to the observer. If the socket is
//! leaked (the heartbeat task holding a clone of the outbound sender keeps
//! the writer alive), the peer entry stays in the DO map and the observer
//! never hears the leave — a ghost peer.
//!
//! ```sh
//! cargo run --release --example leave_repro [backend_url]
//! ```
//! Exit 0 = peer-left arrived (no leak). Exit 1 = timeout (leak).

use std::time::Duration;

use lumen_voice::signaling::{SignalEvent, SignalOut, SignalingClient};
use serde_json::json;

const BACKEND: &str = "https://lumen-backend.renymirelesd.workers.dev";

async fn api(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> anyhow::Result<serde_json::Value> {
    let mut req = client.request(
        reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
        format!("{base}{path}"),
    );
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }
    let res = req.send().await?;
    let status = res.status();
    let text = res.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("{method} {path} -> {status}: {text}");
    }
    Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({})))
}

async fn wait_for(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<SignalEvent>,
    timeout: Duration,
    predicate: impl Fn(&SignalEvent) -> bool,
) -> Option<SignalEvent> {
    tokio::time::timeout(timeout, async {
        while let Some(ev) = rx.recv().await {
            if predicate(&ev) {
                return Some(ev);
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = std::env::args().nth(1).unwrap_or_else(|| BACKEND.to_string());
    let http = reqwest::Client::new();
    let suffix = format!("{:x}", rand::random::<u32>());

    // Register two throwaway users + a server with a voice channel.
    let alice = json!({ "username": format!("repro_a_{suffix}"), "password": "password123" });
    let bob = json!({ "username": format!("repro_b_{suffix}"), "password": "password123" });
    let a = api(&http, &base, "POST", "/api/auth/register", None, Some(alice)).await?;
    let b = api(&http, &base, "POST", "/api/auth/register", None, Some(bob)).await?;
    let (token_a, user_a) = (a["token"].as_str().unwrap(), a["user"]["id"].as_str().unwrap());
    let (token_b, user_b) = (b["token"].as_str().unwrap(), b["user"]["id"].as_str().unwrap());
    println!("registered {user_a} / {user_b}");

    let server = api(&http, &base, "POST", "/api/servers", Some(token_a), Some(json!({"name": format!("repro {suffix}")}))).await?;
    let invite = server["server"]["inviteCode"].as_str().unwrap();
    let channel = server["channels"].as_array().unwrap().iter().find(|c| c["kind"] == "voice").unwrap();
    let channel_id = channel["id"].as_str().unwrap();
    api(&http, &base, "POST", "/api/servers/join", Some(token_b), Some(json!({"inviteCode": invite}))).await?;
    println!("channel {channel_id}");

    let ws_base = base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let ws_url = format!("{ws_base}/api/ws/{channel_id}");

    // Observer (alice) joins first.
    let (_observer, mut obs_rx) = SignalingClient::connect(&ws_url, token_a, channel_id, user_a, "observer").await?;
    let joined_a = wait_for(&mut obs_rx, Duration::from_secs(10), |e| matches!(e, SignalEvent::Joined { .. })).await
        .expect("observer never joined");
    let SignalEvent::Joined { peer_id: observer_id, .. } = joined_a else { unreachable!() };
    println!("observer joined as {observer_id}");

    // Peer (bob) joins; observer must see peer-joined.
    let (peer, mut peer_rx) = SignalingClient::connect(&ws_url, token_b, channel_id, user_b, "peer").await?;
    let joined_b = wait_for(&mut peer_rx, Duration::from_secs(10), |e| matches!(e, SignalEvent::Joined { .. })).await
        .expect("peer never joined");
    let SignalEvent::Joined { peer_id: peer_id_b, .. } = joined_b else { unreachable!() };
    println!("peer joined as {peer_id_b}");

    let seen = wait_for(&mut obs_rx, Duration::from_secs(10), |e| matches!(e, SignalEvent::PeerJoined(p) if p.peer_id == peer_id_b)).await;
    assert!(seen.is_some(), "observer never saw peer-joined");
    println!("observer saw peer-joined for {peer_id_b}");

    // ---- The scenario under test -----------------------------------------
    // This is exactly what VoiceSession::stop() does to the signaling layer:
    // send Close, then drop the session's SignalOut sender. The writer closes
    // the WS → DO broadcasts peer-left.
    let _ = peer.tx.send(SignalOut::Close);
    drop(peer.tx);
    println!("sent Close and dropped peer's sender (leave)");

    // The peer's own socket should end (Closed event)…
    let peer_closed = wait_for(&mut peer_rx, Duration::from_secs(8), |e| matches!(e, SignalEvent::Closed { .. })).await;
    println!("peer saw Closed: {}", peer_closed.is_some());

    // …and the observer should be told the peer left.
    let left = wait_for(&mut obs_rx, Duration::from_secs(15), |e| matches!(e, SignalEvent::PeerLeft(id) if *id == peer_id_b)).await;
    match left {
        Some(_) => {
            println!("RESULT: PASS — peer-left delivered, WS closed on leave");
            Ok(())
        }
        None => {
            println!("RESULT: FAIL — no peer-left in 15s; peer WS stayed open after leave (ghost peer)");
            std::process::exit(1);
        }
    }
}
