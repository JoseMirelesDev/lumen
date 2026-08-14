//! P2P diag for the Lumen voice transport.
//!
//! Modes:
//!   * Two-client (default): joins user1 + user2 in ONE process to the same
//!     channel and measures the RTP path between them. Fine for same-machine
//!     or netns tests where both sides run in one process.
//!   * Single (`--single`): joins ONE client (receive-only with `--no-mic`).
//!     Run one instance on the host (sender, open mic) and a second inside a
//!     network namespace (`sudo ip netns exec lumen2 …`) so its RTP traverses
//!     the veth + tc netem (real packet latency/loss) — simulating a distant
//!     peer without a second machine.
//!
//! Run with LUMEN_VOICE_DIAG=1 to capture the trace:
//!   LUMEN_VOICE_DIAG=1 p2p_diag <backend> <t1> <u1> <t2> <u2> <channel> [--stun-only]
//!   LUMEN_VOICE_DIAG=1 p2p_diag --single [--no-mic] [--stun-only] [--flip-model-at=<secs>] <backend> <token> <user> <channel>

use std::time::Duration;
use lumen_voice::client::{VoiceClient, VoiceJoinArgs, IceServer};

async fn ice_servers(backend: &str, token: &str, stun_only: bool) -> anyhow::Result<Vec<IceServer>> {
    let cfg: serde_json::Value = reqwest::Client::new()
        .get(format!("{backend}/api/realtime/config"))
        .bearer_auth(token)
        .send()
        .await?
        .json()
        .await?;
    let ice = cfg["iceServers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|s| IceServer {
            urls: s["urls"].as_array().cloned().unwrap_or_default()
                .into_iter().filter_map(|u| u.as_str().map(String::from))
                .filter(|u| !stun_only || !u.starts_with("turn")).collect(),
            username: s["username"].as_str().map(String::from),
            credential: s["credential"].as_str().map(String::from),
        })
        .filter(|s| !s.urls.is_empty())
        .collect::<Vec<_>>();
    Ok(ice)
}

fn join_args(backend: &str, token: &str, user: &str, ch: &str, ice: Vec<IceServer>, open_mic: bool, input_wav: Option<String>, open_output: bool) -> VoiceJoinArgs {
    VoiceJoinArgs {
        backend_url: backend.to_string(),
        token: token.to_string(),
        channel_id: ch.to_string(),
        user_id: user.to_string(),
        username: user.to_string(),
        ice_servers: ice,
        open_mic,
        input_wav,
        open_output,
    }
}

fn drain_events(ev1: &mut mpsc::UnboundedReceiver<lumen_voice::VoiceEvent>, who: &str, printed: &mut std::collections::HashSet<String>) {
    while let Ok(ev) = ev1.try_recv() {
        let key = format!("{who}:{ev:?}");
        if printed.insert(key) {
            println!("{who} ev: {ev:?}");
        }
    }
}

use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let single = args.iter().any(|a| a == "--single");
    let no_mic = args.iter().any(|a| a == "--no-mic");
    let no_output = args.iter().any(|a| a == "--no-output");
    let stun_only = args.iter().any(|a| a == "--stun-only");
    let aec_off = args.iter().any(|a| a == "--aec-off");
    // --model=ns-only|fastenhancer (single mode): the suppressor model to run.
    // Defaults to the client default (FastEnhancerM) — matching the app's
    // persisted settings requires passing --model=ns-only explicitly.
    let model = args.iter().find_map(|a| a.strip_prefix("--model=").map(|s| s.to_string()));
    // --input=path (single token: the value must ride with the flag so it is
    // not mistaken for a positional arg).
    let input_wav = args.iter().find_map(|a| a.strip_prefix("--input=").map(|s| s.to_string()));
    // --flip-model-at=<secs> (single mode): flip the suppressor model after N
    // seconds while the session stays joined — the diag `proc` model field
    // flipping (no rejoin, no send-loop exit) proves the live switch.
    let flip_model_at: Option<u64> = args
        .iter()
        .find_map(|a| a.strip_prefix("--flip-model-at="))
        .and_then(|s| s.parse().ok());
    let pos: Vec<String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).cloned().collect();

    if single {
        if pos.len() < 4 {
            eprintln!("usage: p2p_diag --single [--no-mic] [--stun-only] [--flip-model-at=<secs>] <backend> <token> <user> <channel>");
            std::process::exit(2);
        }
        let (backend, tok, user, ch) = (&pos[0], &pos[1], &pos[2], &pos[3]);
        let ice = ice_servers(backend, tok, stun_only).await?;
        let (c, mut ev) = VoiceClient::new();
        if let Some(m) = model.as_deref() {
            use lumen_voice::audio::SuppressorModel;
            let m = match m {
                "ns-only" => SuppressorModel::NsOnly,
                "fastenhancer-s" => SuppressorModel::FastEnhancerS,
                "fastenhancer" => SuppressorModel::FastEnhancerM,
                other => anyhow::bail!("unknown --model '{other}' — use ns-only | fastenhancer-s | fastenhancer"),
            };
            c.set_suppressor_model(m);
        }
        if aec_off {
            c.set_aec_enabled_now(false);
        }
        c.join(join_args(backend, tok, user, ch, ice, !no_mic && input_wav.is_none(), input_wav.clone(), !no_output)).await.map_err(anyhow::Error::msg)?;
        println!("[{user}] joined (mic={}, wav={}, model={model:?}, flip_model_at={flip_model_at:?}) running 60 s...", !no_mic && input_wav.is_none(), input_wav.is_some());
        let mut printed = std::collections::HashSet::new();
        let start = std::time::Instant::now();
        let mut flipped = false;
        while start.elapsed() < Duration::from_secs(60) {
            drain_events(&mut ev, user, &mut printed);
            if !flipped {
                if let Some(t) = flip_model_at {
                    if start.elapsed() >= Duration::from_secs(t) {
                        use lumen_voice::audio::SuppressorModel;
                        let cur = c.suppressor_model();
                        let nxt = match cur {
                            SuppressorModel::FastEnhancerM => SuppressorModel::NsOnly,
                            SuppressorModel::FastEnhancerS => SuppressorModel::NsOnly,
                            SuppressorModel::NsOnly => SuppressorModel::FastEnhancerM,
                        };
                        c.set_suppressor_model(nxt);
                        println!("[{user}] flipping suppressor model {} -> {} at {:.1} s (session stays joined)", cur.as_str(), nxt.as_str(), start.elapsed().as_secs_f64());
                        flipped = true;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        println!("[{user}] done");
        return Ok(());
    }

    if pos.len() < 6 {
        eprintln!("usage: p2p_diag [--stun-only] <backend> <token1> <user1> <token2> <user2> <channel>");
        std::process::exit(2);
    }
    let (backend, t1, u1, t2, u2, ch) = (&pos[0], &pos[1], &pos[2], &pos[3], &pos[4], &pos[5]);
    let ice = ice_servers(backend, t1, stun_only).await?;
    println!("ice servers: {} (turn={}) stun_only={stun_only}", ice.len(),
        ice.iter().filter(|s| s.urls.iter().any(|u| u.starts_with("turn"))).count());
    let (c1, mut ev1) = VoiceClient::new();
    let (c2, mut ev2) = VoiceClient::new();
    println!("joining {} as {u1} first, then {u2} after 3 s...", backend);
    c1.join(join_args(backend, t1, u1, ch, ice.clone(), true, None, true)).await.map_err(anyhow::Error::msg)?;
    println!("user1 joined; waiting 3 s for user2 to see it...");
    tokio::time::sleep(Duration::from_secs(3)).await;
    c2.join(join_args(backend, t2, u2, ch, ice, false, None, !no_output)).await.map_err(anyhow::Error::msg)?;
    println!("user2 joined (no-mic). running 20 s...");
    let mut printed = std::collections::HashSet::new();
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        drain_events(&mut ev1, "user1", &mut printed);
        drain_events(&mut ev2, "user2", &mut printed);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("done. traces: /tmp/lumen-voice-diag-<pid>.jsonl (this process = both clients)");
    Ok(())
}
