//! TEMP DIAG (bug reproduction): join a voice channel as a peer and observe
//! the shared playout buffer mechanics when a remote peer drops off the
//! network. Run three processes on the same channel:
//!
//! ```sh
//! cargo run --release --example drop_peer -- observe <tokenA> <userA> <channel>
//! cargo run --release --example drop_peer -- peer    <tokenB> <userB> <channel>
//! cargo run --release --example drop_peer -- peer    <tokenC> <userC> <channel>
//! ```
//!
//! Then `kill -9` one `peer` process and watch the observer's per-second
//! `buffer` / `dropped` lines: a saturated buffer (several frames) with
//! `dropped` growing while N>1 peers are pushing, collapsing to ~1 frame once
//! the dropped peer's playout task dies — the latency/packet-loss mechanics.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_voice::{VoiceClient, VoiceEvent, VoiceJoinArgs};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: drop_peer <observe|peer> <token> <user_id> <channel_id> [backend_url]");
        std::process::exit(2);
    }
    let role = a[1].clone();
    let token = a[2].clone();
    let user_id = a[3].clone();
    let channel_id = a[4].clone();
    let backend = a
        .get(5)
        .cloned()
        .unwrap_or_else(|| "https://lumen-backend.renymirelesd.workers.dev".to_string());

    let (client, mut events) = VoiceClient::new();
    let client = std::sync::Arc::new(client);
    let started = Instant::now();
    client
        .join(VoiceJoinArgs {
            backend_url: backend,
            token,
            channel_id,
            user_id,
            ice_servers: vec![],
        })
        .await
        .map_err(|e| format!("join failed: {e}"))?;
    println!("[{role}] joined at t=0");

    let mut peer_count = 0usize;
    // Reporter runs on its own task: the 10 Hz Levels stream would otherwise
    // win every select tick and starve a timer branch in the same select.
    if role == "observe" {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Some((frames, dropped)) = client.output_stats().await {
                    println!(
                        "[observe] t={:.0}s buffer={frames:2} frames shed={dropped:>8}",
                        started.elapsed().as_secs_f32()
                    );
                }
            }
        });
    }
    loop {
        tokio::select! {
            ev = events.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    VoiceEvent::PeerJoined { peer_id, .. } => {
                        peer_count += 1;
                        println!("[{role}] t={:.0}s peer joined ({peer_count} peers)", started.elapsed().as_secs_f32());
                    }
                    VoiceEvent::PeerLeft { .. } => {
                        peer_count = peer_count.saturating_sub(1);
                        println!("[{role}] t={:.0}s peer left ({peer_count} peers)", started.elapsed().as_secs_f32());
                    }
                    VoiceEvent::State { peer_id, state } => {
                        println!("[{role}] t={:.0}s state {peer_id:?} -> {state:?}", started.elapsed().as_secs_f32());
                    }
                    VoiceEvent::Signaling { .. } => {
                        println!("[{role}] t={:.0}s signaling closed", started.elapsed().as_secs_f32());
                        break;
                    }
                    VoiceEvent::Error { message, .. } => {
                        println!("[{role}] t={:.0}s error: {message}", started.elapsed().as_secs_f32());
                    }
                    _ => {}
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                // fallthrough: select keeps polling events
            }
        }
    }
    println!("[{role}] exiting");
    Ok(())
}
