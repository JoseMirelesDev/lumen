//! Native voice: the entire audio path runs here, not in the WebView.
//!
//! ```text
//! cpal mic ─▶ opus encode ─▶ RTP ─▶ webrtc-rs ─▶ UDP (SRTP)
//! UDP (SRTP) ─▶ webrtc-rs ─▶ jitter buffer ─▶ opus decode ─▶ cpal out
//!                 ▲                                        ▲
//!                 └─ tokio-tungstenite WS ◀─ DO relay ◀────┘
//! ```
//!
//! Screen/video streaming is deliberately not implemented: [`client`] is
//! generic over track kind (audio now), and the negotiation path in
//! `client.rs` (`add_track` → renegotiation) is the seam where a video
//! `TrackLocalStaticRTP` will slot in later.

pub mod audio;
pub mod client;
pub mod rtp;
pub mod signaling;

pub use client::{VoiceClient, VoiceJoinArgs};
