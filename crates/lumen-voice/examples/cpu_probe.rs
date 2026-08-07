//! CPU probe: join a voice channel via VoiceClient (the exact same path the
//! Slint client uses) and idle for N seconds, capturing mic audio and running
//! the full send pipeline (NS + GTCRN + Opus + WebRTC). Prints per-thread CPU
//! at the end so we can attribute cost to each subsystem.
//!
//! Usage: cargo run --release --example cpu_probe -- <token> <user_id> <channel_id> [backend_url] [seconds]
//!
//! The default backend is http://localhost:8787 and duration is 30s.

use std::time::{Duration, Instant};
use lumen_voice::{VoiceClient, VoiceEvent, VoiceJoinArgs};

/// Read /proc/<pid>/task/*/stat and return (tid, comm, utime+stime) per thread.
fn thread_ticks(pid: u32) -> Vec<(u32, String, u64)> {
    let task_dir = format!("/proc/{pid}/task");
    let Ok(entries) = std::fs::read_dir(&task_dir) else { return vec![] };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let tid: u32 = match entry.file_name().to_str().and_then(|s| s.parse().ok()) {
            Some(t) => t,
            None => continue,
        };
        let stat_path = format!("{}/{tid}/stat", task_dir);
        let Ok(stat) = std::fs::read_to_string(&stat_path) else { continue };
        // comm is between first '(' and last ')' — may contain spaces.
        let comm_start = stat.find('(').unwrap_or(0) + 1;
        let comm_end = stat.rfind(')').unwrap_or(stat.len());
        let comm = stat[comm_start..comm_end].to_string();
        // Fields after ')': state(1) ppid(2) ... utime(12) stime(13) — 1-indexed from after comm.
        let rest: Vec<&str> = stat[comm_end + 2..].split_whitespace().collect();
        if rest.len() < 13 { continue; }
        let utime: u64 = rest[11].parse().unwrap_or(0); // field 14 overall
        let stime: u64 = rest[12].parse().unwrap_or(0); // field 15 overall
        out.push((tid, comm, utime + stime));
    }
    out
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("usage: cpu_probe <token> <user_id> <channel_id> [backend_url] [seconds]");
        std::process::exit(1);
    }
    let token = a[1].clone();
    let user_id = a[2].clone();
    let channel_id = a[3].clone();
    let backend_url = a.get(4).cloned().unwrap_or_else(|| "http://localhost:8787".to_string());
    let seconds: u64 = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(30);

    let pid = std::process::id();
    let clk_tck: u64 = 100; // standard Linux CLK_TCK
    println!("pid={pid}, CLK_TCK={clk_tck}");

    let (client, mut events) = VoiceClient::new();
    let client = std::sync::Arc::new(client);

    // Drain events in background
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match &ev {
                VoiceEvent::Error { message, .. } => eprintln!("voice error: {message}"),
                VoiceEvent::Debug { message, .. } => println!("  debug: {message}"),
                VoiceEvent::Signaling { state } => println!("  signaling: {state:?}"),
                VoiceEvent::State { peer_id, state } => println!("  peer {peer_id}: {state:?}"),
                VoiceEvent::PeerJoined { peer_id, username, .. } => println!("  peer joined: {username} ({peer_id})"),
                VoiceEvent::PeerLeft { peer_id } => println!("  peer left: {peer_id}"),
                _ => {} // Levels: ignore (10Hz spam)
            }
        }
    });

    // Snapshot BEFORE join
    let snap_before = thread_ticks(pid);
    let t0 = Instant::now();

    client
        .join(VoiceJoinArgs {
            backend_url,
            token,
            channel_id,
            user_id,
            username: "cpu_probe".to_string(),
            ice_servers: vec![],
        })
        .await
        .map_err(|e| format!("join failed: {e}"))?;

    // When MUTED=1, skip NS+encode to isolate non-DSP overhead (WebRTC/tokio/audio).
    if std::env::var("MUTED").as_deref() == Ok("1") {
        client.set_muted(true).await;
        println!("  ** MUTED mode: NS/encode skipped, measuring overhead only **");
    }

    println!("joined — capturing mic + running send pipeline for {seconds}s...");

    // Let it run for N seconds
    tokio::time::sleep(Duration::from_secs(seconds)).await;

    // Snapshot AFTER
    let elapsed = t0.elapsed().as_secs_f64();
    let snap_after = thread_ticks(pid);

    // Compute deltas
    let before_map: std::collections::HashMap<u32, (String, u64)> =
        snap_before.into_iter().map(|(t, c, v)| (t, (c, v))).collect();
    let mut results: Vec<(String, f64)> = Vec::new();
    let mut total_pct = 0.0;
    for (tid, comm, after) in &snap_after {
        let before_val = before_map.get(tid).map(|(_, v)| *v).unwrap_or(0);
        let delta = after.saturating_sub(before_val);
        let pct = (delta as f64 / clk_tck as f64 / elapsed) * 100.0;
        total_pct += pct;
        if pct > 0.01 {
            results.push((format!("{tid} {comm}"), pct));
        }
    }
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    println!("\n=== per-thread CPU over {elapsed:.1}s (% of one core) ===");
    for (label, pct) in &results {
        println!("  {pct:6.2}%  {label}");
    }
    println!("  ------");
    println!("  {total_pct:6.2}%  TOTAL (sum of all threads = ~process CPU)");
    let cores = num_cpus();
    if cores > 0 {
        println!("  {:.2}%  of all {cores} cores (what Task Manager shows)", total_pct / cores as f64);
    }

    println!("\nleaving...");
    client.leave().await;
    println!("done.");
    Ok(())
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|p| p.get()).unwrap_or(1)
}
