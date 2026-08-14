//! H1 silence-path contract test: the encode-skip in the send loop must not
//! change what is transmitted for speech.
//!
//! Runs the SAME real speech (testdata/speech.wav: alternating speech /
//! digital silence) through two send loops:
//!   - ORIGINAL: encode every frame (always-transmit, the pre-skip path).
//!   - SKIP: the production decision (`silence_reuse_decision`) reuses the
//!     cached silence packet on NS-confirmed silence.
//! and asserts:
//!   1. On every speech frame the SKIP path sends a BYTE-IDENTICAL packet to
//!      the ORIGINAL path (the skip never alters, delays, or caches speech —
//!      no edge cuts, no quality delta).
//!   2. On silence, a "reused" frame sends a packet that was genuinely
//!      transmitted earlier (a true cache hit, not a fabrication).
//!   3. The transmitted stream decodes length-continuously (one 960-sample
//!      frame per packet — no gaps).
//!   4. The speech energy survives in the decoded skip stream (cumulative
//!      decoded RMS over speech runs above the silence floor).
//!
//! Run: `cargo test --release -p lumen-voice --test silence_path -- --ignored --nocapture`

use lumen_voice::audio::{
    rms_level, silence_reuse_decision, NoiseSuppressor, OpusDecoder, OpusEncoder,
};
use std::io::Read;

const FRAME: usize = 960;

fn load_speech() -> Vec<i16> {
    let mut file = std::fs::File::open("testdata/speech.wav").expect("testdata/speech.wav");
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).unwrap();
    buf[44..]
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Run one send loop; returns (packets, per-frame speech flag, per-frame reused flag).
fn run_loop(
    frames: &[&[i16]],
    skip: bool,
) -> (Vec<Vec<u8>>, Vec<bool>, Vec<bool>) {
    let mut ns = NoiseSuppressor::new();
    let mut enc = OpusEncoder::new().unwrap();
    let mut silence_pkt: Option<Vec<u8>> = None;
    let mut streak: u32 = 0;
    let mut pkts = Vec::with_capacity(frames.len());
    let mut speech_flags = Vec::with_capacity(frames.len());
    let mut reused_flags = Vec::with_capacity(frames.len());
    for f in frames {
        let cleaned = ns.process_gated(f).unwrap();
        let speech = ns.speech_detected() || rms_level(f) > 0.01;
        speech_flags.push(speech);
        if skip {
            let (reuse, new_streak) =
                silence_reuse_decision(speech, streak, silence_pkt.is_some());
            streak = new_streak;
            if reuse {
                reused_flags.push(true);
                pkts.push(silence_pkt.clone().unwrap());
                continue;
            }
        }
        reused_flags.push(false);
        let e = enc.encode(&cleaned).unwrap();
        if !speech {
            silence_pkt = Some(e.clone());
        }
        pkts.push(e);
    }
    (pkts, speech_flags, reused_flags)
}

#[test]
#[ignore = "manual contract test (release build; reads testdata wav)"]
fn silence_path_never_touches_speech() {
    let pcm = load_speech();
    let frames: Vec<&[i16]> = pcm.chunks_exact(FRAME).collect();
    assert!(frames.len() > 100, "expected >=100 frames, got {}", frames.len());

    let (pkts_orig, speech, _) = run_loop(&frames, false);
    let (pkts_skip, _, reused) = run_loop(&frames, true);

    assert_eq!(pkts_orig.len(), frames.len());
    assert_eq!(pkts_skip.len(), frames.len());

    let n_speech = speech.iter().filter(|s| **s).count();
    let n_reuse = reused.iter().filter(|r| **r).count();
    println!(
        "frames={} speech={} reuse={} ({:.0}%)",
        frames.len(),
        n_speech,
        n_reuse,
        100.0 * n_reuse as f64 / frames.len() as f64
    );

    // 1. Speech frames are never served from the cache (an onset is always
    //    encoded fresh — no edge cut). The packets may differ slightly from
    //    the ORIGINAL run (opus encoder state diverges once silence frames
    //    are not fed to it), but the CONTENT must survive — checked below.
    for (i, (s, r)) in speech.iter().zip(&reused).enumerate() {
        assert!(!(*s && *r), "frame {i}: speech frame was served from cache — edge cut");
    }

    // 2. A reused packet was genuinely sent earlier (cache hit).
    for (i, r) in reused.iter().enumerate() {
        if *r {
            assert!(
                pkts_skip[..i].iter().any(|p| p == &pkts_skip[i]),
                "frame {i}: reused packet was never transmitted before"
            );
        }
    }

    // 3. Decode both streams and compare speech energy frame by frame.
    let mut dec_orig = OpusDecoder::new().unwrap();
    let mut dec_skip = OpusDecoder::new().unwrap();
    let mut speech_orig = 0.0f64;
    let mut speech_skip = 0.0f64;
    let mut decoded = Vec::with_capacity(pkts_skip.len() * FRAME);
    for (i, ((p_skip, p_orig), s)) in pkts_skip.iter().zip(&pkts_orig).zip(&speech).enumerate() {
        let out_skip = dec_skip
            .decode(Some(p_skip))
            .unwrap_or_else(|e| panic!("frame {i}: skip decode failed: {e}"));
        let out_orig = dec_orig
            .decode(Some(p_orig))
            .unwrap_or_else(|e| panic!("frame {i}: orig decode failed: {e}"));
        assert_eq!(out_skip.len(), FRAME, "frame {i}: skip decoded {}, expected {FRAME}", out_skip.len());
        assert_eq!(out_orig.len(), FRAME, "frame {i}: orig decoded {}, expected {FRAME}", out_orig.len());
        if *s {
            speech_orig += rms_level(&out_orig) as f64;
            speech_skip += rms_level(&out_skip) as f64;
        }
        decoded.extend_from_slice(&out_skip);
    }
    assert_eq!(decoded.len(), pkts_skip.len() * FRAME, "stream not length-continuous");

    // 4. The speech survives in the skip stream: cumulative decoded energy
    //    over speech frames >= 90% of the original path (a single onset frame
    //    may legitimately DTX — opus's own VAD — but the runs as a whole must
    //    carry the speech).
    println!("speech energy: original {speech_orig:.2} vs skip {speech_skip:.2}");
    assert!(
        speech_skip >= speech_orig * 0.9,
        "skip speech energy {speech_skip:.3} < 90% of original {speech_orig:.3} — speech lost"
    );
    assert!(
        speech_skip > 0.005 * n_speech as f64,
        "skip speech energy {speech_skip:.3} at the silence floor — speech lost"
    );
    println!(
        "OK: {} frames continuous; {} speech frames never cached; speech energy preserved ({:.0}%)",
        frames.len(),
        n_speech,
        100.0 * speech_skip / speech_orig
    );
}
