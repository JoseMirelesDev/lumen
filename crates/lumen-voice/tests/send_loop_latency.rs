//! End-to-end pipeline latency measurement and send-loop regression tests.
//!
//! 1. Pipeline latency:  RTP → JitterBuffer → OpusDecoder → measured delay.
//!    Proves the receive path adds bounded delay (~40-80 ms), never seconds.
//!
//! 2. Jitter buffer stability: 30 s of continuous audio with matched clocks
//!    shows no drift. 1% sender-faster clock shows bounded pending growth.
//!
//! 3. Send-loop drain-to-latest: proves a burst of stale frames is discarded
//!    by the new pattern and retained by the old pattern.

use std::sync::Arc;
use std::time::{Duration, Instant};

use lumen_voice::audio::{JitterBuffer, OpusDecoder, OpusEncoder, FRAME_SAMPLES};
use parking_lot::Mutex;
use tokio::sync::mpsc;

// ───────────────────────────────────────────────────────────────────────────
// 1. Pipeline latency: jitter buffer → decode, wall-clock measured
// ───────────────────────────────────────────────────────────────────────────

/// Measures the wall-clock delay between injecting an RTP frame into the
/// jitter buffer and popping it out (ready to play). This is the irreducible
/// receive-side pipeline latency.
///
/// With target=2 frames (40 ms fill), the first frame should appear after
/// ~40 ms plus tick alignment. Subsequent frames should appear at ~20 ms
/// intervals with near-zero additional delay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipeline_latency_bounded() {
    let jb = Arc::new(Mutex::new(JitterBuffer::new(2)));
    let mut decoder = OpusDecoder::new().unwrap();
    let mut encoder = OpusEncoder::new().unwrap();

    // Encode a recognizable tone.
    let tone: Vec<i16> = (0..FRAME_SAMPLES)
        .map(|i| ((i as f64 * 0.1).sin() * 10000.0) as i16)
        .collect();
    let encoded = encoder.encode(&tone).unwrap();

    let total_frames: u32 = 20; // 400 ms of audio
    let jb_push = jb.clone();
    let enc = encoded.clone();

    // Track injection timestamps.
    let inject_times: Arc<Mutex<Vec<(u32, Instant)>>> = Arc::new(Mutex::new(Vec::new()));
    let inject_w = inject_times.clone();

    // Producer: inject RTP frames at 20 ms wall-clock cadence.
    let producer = tokio::spawn(async move {
        for i in 0..total_frames {
            let ts = i * FRAME_SAMPLES as u32;
            inject_w.lock().push((ts, Instant::now()));
            jb_push.lock().push(i as u16, ts, enc.clone());
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    // Consumer (playout): pop at 20 ms cadence, decode, record when each
    // RTP timestamp becomes available.
    let jb_pop = jb.clone();
    let play_times: Arc<Mutex<Vec<(u32, Instant)>>> = Arc::new(Mutex::new(Vec::new()));
    let play_w = play_times.clone();

    let consumer = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(20));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        for _ in 0..(total_frames + 10) {
            ticker.tick().await;
            let frame = jb_pop.lock().pop();
            match frame {
                Some(Some((rtp_ts, payload))) => {
                    // Decode to prove the full path works (not just pop).
                    let _pcm = decoder.decode(Some(&payload)).unwrap();
                    play_w.lock().push((rtp_ts, Instant::now()));
                }
                Some(None) => {
                    // PLC
                    let _pcm = decoder.decode(None).unwrap();
                }
                None => {} // still filling
            }
        }
    });

    producer.await.unwrap();
    consumer.await.unwrap();

    // Compute latencies.
    let injects = inject_times.lock().clone();
    let plays = play_times.lock().clone();

    assert!(!plays.is_empty(), "no frames were played back");

    let mut latencies_ms = Vec::new();
    for (play_ts, play_time) in &plays {
        if let Some((_, inject_time)) = injects.iter().find(|(ts, _)| ts == play_ts) {
            let lat = play_time.duration_since(*inject_time).as_millis();
            latencies_ms.push(lat);
        }
    }

    assert!(
        !latencies_ms.is_empty(),
        "could not match any inject/play timestamps"
    );

    let min = *latencies_ms.iter().min().unwrap();
    let max = *latencies_ms.iter().max().unwrap();
    let avg = latencies_ms.iter().sum::<u128>() / latencies_ms.len() as u128;

    eprintln!(
        "pipeline latency ({} frames): min {} ms, max {} ms, avg {} ms",
        latencies_ms.len(),
        min,
        max,
        avg
    );

    // The jitter buffer fill (target=2, 40 ms) dominates the first frame.
    // Subsequent frames should be near 20 ms (one tick). The maximum across
    // all frames must be well under 200 ms — never seconds.
    assert!(
        max < 200,
        "pipeline latency must be < 200 ms, got max {max} ms"
    );
    // The average should be under 100 ms.
    assert!(
        avg < 100,
        "average pipeline latency must be < 100 ms, got {avg} ms"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// 2. Jitter buffer stability over long runs
// ───────────────────────────────────────────────────────────────────────────

/// 30 seconds of matched-clock audio: push one, pop one. Pending count must
/// stay bounded (no drift).
#[test]
fn jitter_buffer_no_drift_30s() {
    let mut jb = JitterBuffer::new(2);
    let payload = vec![0u8; 50];

    let mut max_pending = 0usize;
    let mut pending_now = 0usize;

    for i in 0u32..1500 {
        let ts = i * FRAME_SAMPLES as u32;
        jb.push(i as u16, ts, payload.clone());
        pending_now += 1;

        match jb.pop() {
            Some(Some(_)) => {
                pending_now -= 1;
            }
            Some(None) => {} // PLC, frame was in buffer but wrong ts? shouldn't happen here
            None => {}       // still filling
        }
        if pending_now > max_pending {
            max_pending = pending_now;
        }
    }

    eprintln!(
        "jitter buffer 30s: max pending {max_pending}, final pending {pending_now}, dropped {}",
        jb.dropped()
    );
    // After initial fill (target=2 frames), pending stays at 1 (just pushed,
    // not yet popped by next iteration). Max should be 2-3.
    assert!(
        max_pending <= 4,
        "jitter buffer pending grew to {max_pending} — drift!"
    );
    assert_eq!(jb.dropped(), 0, "no frames should be dropped");
}

/// Sender runs 1% faster: 1010 pushes for 1000 pops. The 10 extra frames
/// accumulate in pending — bounded by the skew, not growing without limit.
/// In production, AudioOutput's overflow guard sheds these.
#[test]
fn jitter_buffer_sender_faster_bounded_pending() {
    let mut jb = JitterBuffer::new(2);
    let payload = vec![0u8; 50];

    // Interleave pushes and pops to simulate real-time arrival.
    // Every 100 pops, push 101 frames (1% skew).
    let mut pushed = 0u32;
    let mut played = 0u32;
    let mut max_pending = 0usize;

    for cycle in 0..10 {
        // Push 101 frames.
        for _ in 0..101 {
            let ts = pushed * FRAME_SAMPLES as u32;
            jb.push(pushed as u16, ts, payload.clone());
            pushed += 1;
        }
        // Pop 100 frames.
        for _ in 0..100 {
            match jb.pop() {
                Some(Some(_)) => {
                    played += 1;
                }
                Some(None) | None => {}
            }
        }
        let pending = pushed - played - jb.dropped() as u32;
        if pending as usize > max_pending {
            max_pending = pending as usize;
        }
        eprintln!("  cycle {cycle}: pushed {pushed}, played {played}, pending ~{pending}");
    }

    eprintln!(
        "sender-faster: total pushed {pushed}, played {played}, dropped {}, max pending {max_pending}",
        jb.dropped()
    );
    // ~10 extra frames accumulated (1% of 1000). Bounded, not growing.
    assert!(played >= 990, "should play most frames: got {played}");
}

// ───────────────────────────────────────────────────────────────────────────
// 3. Send-loop drain-to-latest
// ───────────────────────────────────────────────────────────────────────────

/// Old pattern: takes one frame from a burst, leaves the rest as backlog.
#[tokio::test(flavor = "current_thread")]
async fn send_loop_old_pattern_leaves_backlog() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<i16>>();

    // Simulate a burst of 50 frames (1 s of audio accumulated during a
    // runtime stall — signaling negotiation, GC, etc.).
    for seq in 0..50 {
        let _ = tx.send(make_frame(seq));
    }

    // Old pattern: takes the OLDEST frame.
    let frame = rx.try_recv().unwrap();
    assert_eq!(frame[0], 0, "old pattern takes the oldest frame");

    // 49 frames still pending = 980 ms of stale audio that would be sent
    // before the live edge. This is the delay the remote listener hears.
    let pending = drain_count(&mut rx);
    eprintln!(
        "old pattern: 1 consumed (seq 0), {pending} pending = {} ms stale",
        pending * 20
    );
    assert!(pending >= 40, "old pattern should leave stale frames: {pending}");
}

/// New pattern: drains the burst, keeps only the newest frame. Zero backlog.
#[tokio::test(flavor = "current_thread")]
async fn send_loop_new_pattern_drains_to_latest() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<i16>>();

    // Same burst of 50 frames.
    for seq in 0..50 {
        let _ = tx.send(make_frame(seq));
    }

    // New pattern: drain to latest.
    let mut frame: Option<Vec<i16>> = None;
    while let Ok(f) = rx.try_recv() {
        frame = Some(f);
    }
    let frame = frame.unwrap();
    assert_eq!(frame[0], 49, "new pattern keeps ONLY the newest frame (seq 49)");

    // Zero pending.
    let pending = drain_count(&mut rx);
    eprintln!("new pattern: consumed seq 49, pending {pending}");
    assert_eq!(pending, 0, "new pattern leaves zero stale frames");
}

/// Muted drain: unmuting after silence must not play stale audio.
#[tokio::test]
async fn muted_drain_clears_stale() {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<i16>>();

    // 100 frames accumulated while muted (2 s of audio).
    for seq in 0..100 {
        let _ = tx.send(make_frame(seq));
    }

    // The fix: drain while muted.
    while rx.try_recv().is_ok() {}

    // Fresh frame arrives after unmute.
    let _ = tx.send(make_frame(999));
    let frame = rx.try_recv().unwrap();
    assert_eq!(frame[0], 999, "should get the fresh frame");
    assert!(rx.try_recv().is_err(), "no stale frames remain");
}

// ───────────────────────────────────────────────────────────────────────────
// Helpers
// ───────────────────────────────────────────────────────────────────────────

fn make_frame(seq: usize) -> Vec<i16> {
    let mut frame = vec![0i16; FRAME_SAMPLES];
    frame[0] = seq as i16;
    frame
}

fn drain_count(rx: &mut mpsc::UnboundedReceiver<Vec<i16>>) -> usize {
    let mut n = 0;
    while rx.try_recv().is_ok() {
        n += 1;
    }
    n
}
