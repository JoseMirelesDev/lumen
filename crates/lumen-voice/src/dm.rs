//! DM data-only peer connection (ADR-006, Fase 3): a WebRTC peer connection
//! with NO audio tracks — just a "dm" DataChannel — used for real-time DM
//! typing/delivery between two online friends. Signaling rides the presence
//! WS relay (`dm-signal`, PresenceHubDO), never the server's media path.
//!
//! This module owns only the WebRTC mechanics; the host bridges signaling:
//!   outbound:  DmSignalOut via the returned mpsc receiver → presence WS
//!   inbound:   host calls `handle_signal` with presence-relayed frames
//!   messages:  DmEvent::Message via the events channel

use std::sync::Arc;

use bytes::BytesMut;
use tokio::sync::mpsc;
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::peer_connection::configuration::{RTCConfigurationBuilder, RTCIceServer};
use rtc::peer_connection::sdp::RTCSessionDescription;
use webrtc::data_channel::{DataChannel, DataChannelEvent, RTCDataChannelMessage};
use webrtc::peer_connection::{PeerConnection, PeerConnectionBuilder};

use crate::IceServer;

/// Outbound DM signaling frames (host → presence WS `dm-signal`).
#[derive(Debug, Clone)]
pub enum DmSignalOut {
    Offer(String),
    Answer(String),
    Ice(serde_json::Value),
}

/// Inbound DM signaling frames (presence WS relay → this session).
#[derive(Debug, Clone)]
pub enum DmSignalIn {
    Offer { sdp: String },
    Answer { sdp: String },
    Ice { candidate: serde_json::Value },
}

/// Events this session emits to the host.
#[derive(Debug, Clone)]
pub enum DmEvent {
    Open,
    Message(Vec<u8>),
    Closed,
    Error(String),
}

pub struct DmDataChannel {
    pc: Arc<dyn PeerConnection>,
    dc: Arc<dyn DataChannel>,
}

/// A DM data-channel session plus the outbound signaling stream. The session
/// must be polled: the host drains `rx` and forwards frames to the presence
/// relay; inbound frames arrive via `handle_signal`.
pub struct DmSession {
    pub channel: Arc<DmDataChannel>,
    /// Clone for the host to emit outbound signaling at will.
    pub signal_tx: mpsc::UnboundedSender<DmSignalOut>,
    pub rx: mpsc::UnboundedReceiver<DmSignalOut>,
}

impl DmDataChannel {
    /// Establish a data-only peer connection with a "dm" data channel and
    /// start the message poller. The caller must generate an offer and send
    /// it to the peer (offer via `signal_tx` in the returned stream).
    pub async fn open(
        ice_servers: &[IceServer],
        events: mpsc::UnboundedSender<DmEvent>,
    ) -> anyhow::Result<DmSession> {
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?; // needed for the SDP m-lines
        let config = RTCConfigurationBuilder::new()
            .with_ice_servers(
                ice_servers
                    .iter()
                    .map(|s| webrtc::peer_connection::RTCIceServer {
                        urls: s.urls.clone(),
                        username: s.username.clone().unwrap_or_default(),
                        credential: s.credential.clone().unwrap_or_default(),
                        ..Default::default()
                    })
                    .collect(),
            )
            .build();

        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(config)
                .with_media_engine(media_engine)
                .with_runtime(webrtc::runtime::default_runtime().expect("runtime-tokio feature"))
                .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
                .build()
                .await?,
        );
        let dc = pc.create_data_channel("dm", None).await?;
        let dc_poller = dc.clone();
        let events_poller = events.clone();
        tokio::spawn(async move {
            while let Some(ev) = dc_poller.poll().await {
                match ev {
                    DataChannelEvent::OnOpen => {
                        let _ = events_poller.send(DmEvent::Open);
                    }
                    DataChannelEvent::OnMessage(msg) => {
                        let _ = events_poller.send(DmEvent::Message(msg.data.to_vec()));
                    }
                    DataChannelEvent::OnClose => {
                        let _ = events_poller.send(DmEvent::Closed);
                        break;
                    }
                    _ => {}
                }
            }
        });
        let (signal_tx, rx) = mpsc::unbounded_channel::<DmSignalOut>();
        Ok(DmSession {
            channel: Arc::new(DmDataChannel { pc, dc }),
            signal_tx: signal_tx.clone(),
            rx,
        })
    }

    /// Handle an inbound signaling frame (from the presence relay).
    pub async fn handle_signal(&self, signal: DmSignalIn, signal_tx: &mpsc::UnboundedSender<DmSignalOut>) -> anyhow::Result<()> {
        match signal {
            DmSignalIn::Offer { sdp } => {
                let desc = RTCSessionDescription::offer(sdp)?;
                self.pc.set_remote_description(desc).await?;
                let answer = self.pc.create_answer(None).await?;
                self.pc.set_local_description(answer.clone()).await?;
                let _ = signal_tx.send(DmSignalOut::Answer(answer.sdp));
            }
            DmSignalIn::Answer { sdp } => {
                let desc = RTCSessionDescription::answer(sdp)?;
                self.pc.set_remote_description(desc).await?;
            }
            DmSignalIn::Ice { candidate } => {
                match serde_json::from_value::<webrtc::peer_connection::RTCIceCandidateInit>(candidate) {
                    Ok(init) => self.pc.add_ice_candidate(init).await?,
                    Err(_) => {}
                }
            }
        }
        Ok(())
    }

    /// Create an offer for the remote peer and emit it via `signal_tx`.
    pub async fn create_offer(&self, signal_tx: &mpsc::UnboundedSender<DmSignalOut>) -> anyhow::Result<()> {
        let offer = self.pc.create_offer(None).await?;
        self.pc.set_local_description(offer.clone()).await?;
        let _ = signal_tx.send(DmSignalOut::Offer(offer.sdp));
        Ok(())
    }

    /// Send a text frame over the dm channel.
    pub async fn send(&self, text: &str) -> anyhow::Result<()> {
        self.dc.send(BytesMut::from(text.as_bytes())).await?;
        Ok(())
    }

    pub async fn close(&self) {
        let _ = self.pc.close().await;
    }
}
