//! Native voice for Lumen — framework-agnostic (no Tauri, no UI).
//!
//! ```text
//! cpal mic ─▶ opus encode ─▶ RTP ─▶ webrtc-rs ─▶ UDP (SRTP)
//! UDP (SRTP) ─▶ webrtc-rs ─▶ jitter buffer ─▶ opus decode ─▶ cpal out
//!                 ▲                                        ▲
//!                 └─ tokio-tungstenite WS ◀─ DO relay ◀────┘
//! ```
//!
//! The only output is [`VoiceEvent`] on a channel owned by the host; adapters
//! (Tauri today, Slint next) bridge it to the UI. Screen/video is deliberately
//! not implemented: [`client`] is generic over track kind and the negotiation
//! path in `client.rs` (`add_track` → re-offer) is the seam where a video
//! `TrackLocalStaticRTP` will slot in later.

pub mod audio;
pub mod client;
pub mod dm;
pub mod event;
pub mod rtp;
pub mod signaling;

pub use client::{IceServer, TransmitMode, VoiceClient, VoiceJoinArgs};
pub use event::{PeerLevel, PeerState, SignalingState, VoiceEvent};
