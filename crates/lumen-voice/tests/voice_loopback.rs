//! Live in-process loopback test: two webrtc-rs peer connections negotiate over
//! in-memory signaling (offer/answer + trickle ICE) and carry a real OPUS RTP
//! packet end to end. This proves the native WebRTC transport actually moves
//! audio on this host (the piece that replaces the broken WebKitGTK RTC).
//!
//! Runs only with `cargo test -- --ignored voice_loopback` because it binds
//! UDP sockets and performs real DTLS/SRTP negotiation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lumen_voice::audio::{FRAME_SAMPLES, OpusEncoder};
use lumen_voice::rtp::AudioPacketizer;
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::state::RTCPeerConnectionState;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use tokio::sync::mpsc;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCPeerConnectionIceEvent,
};
use webrtc::peer_connection::RTCIceCandidateInit;
use webrtc::rtp_transceiver::RtpSender;

/// Event handler for one side of the loopback. It sends its ICE candidates to
/// the peer, forwards received RTP payloads to a channel, and flags Connected.
#[derive(Clone)]
struct Handler {
    to_peer: mpsc::UnboundedSender<RTCIceCandidateInit>,
    packets: mpsc::UnboundedSender<Vec<u8>>,
    connected: Arc<AtomicBool>,
}

#[async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            let _ = self.to_peer.send(init);
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        if state == RTCPeerConnectionState::Connected {
            self.connected.store(true, Ordering::SeqCst);
        }
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let tx = self.packets.clone();
        tokio::spawn(async move {
            while let Some(event) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(pkt) = event {
                    let _ = tx.send(pkt.payload.to_vec());
                }
            }
        });
    }
}

/// Build a peer with a single OPUS send track, an in-memory handler, and the
/// local loopback UDP listener.
async fn build_peer(
    handler: Arc<Handler>,
    ssrc: u32,
) -> (Arc<dyn PeerConnection>, Arc<TrackLocalStaticRTP>, Arc<dyn RtpSender>) {
    let mut media_engine = MediaEngine::default();
    media_engine.register_default_codecs().unwrap();
    let registry = register_default_interceptors(Registry::new(), &mut media_engine).unwrap();
    let config = RTCConfigurationBuilder::new().build();

    let track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
        format!("stream-{ssrc}"),
        format!("audio-{ssrc}"),
        "mic".to_string(),
        RtpCodecKind::Audio,
        vec![RTCRtpEncodingParameters {
            rtp_coding_parameters: RTCRtpCodingParameters {
                ssrc: Some(ssrc),
                ..Default::default()
            },
            codec: RTCRtpCodec {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            ..Default::default()
        }],
    )));

    let runtime = webrtc::runtime::default_runtime().expect("runtime-tokio");
    let pc: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_runtime(runtime)
            .with_udp_addrs(vec!["127.0.0.1:0".to_string()])
            .build()
            .await
            .unwrap(),
    );
    let sender = pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await.unwrap();
    (pc, track, sender)
}

async fn wait_connected(a: &Handler, b: &Handler) {
    for _ in 0..100 {
        if a.connected.load(Ordering::SeqCst) && b.connected.load(Ordering::SeqCst) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("peer connections never became Connected on loopback");
}

#[ignore] // requires UDP sockets + real DTLS/SRTP negotiation
#[tokio::test(flavor = "multi_thread")]
async fn voice_loopback() {
    // Candidate plumbing: A's candidates feed B, B's feed A.
    let (cand_a_tx, mut cand_a_rx) = mpsc::unbounded_channel();
    let (cand_b_tx, mut cand_b_rx) = mpsc::unbounded_channel();
    let (_b_packets_tx, _b_packets_rx) = mpsc::unbounded_channel();
    let (b_packets_tx, mut b_packets_rx) = mpsc::unbounded_channel();

    let handler_a = Arc::new(Handler {
        to_peer: cand_a_tx,
        packets: _b_packets_tx,
        connected: Arc::new(AtomicBool::new(false)),
    });
    let handler_b = Arc::new(Handler {
        to_peer: cand_b_tx,
        packets: b_packets_tx,
        connected: Arc::new(AtomicBool::new(false)),
    });

    let (pc_a, track_a, _sender_a) = build_peer(handler_a.clone(), 0xaaaa_0001).await;
    let (pc_b, _track_b, _sender_b) = build_peer(handler_b.clone(), 0xbbbb_0002).await;

    // Glare-free negotiation: A offers, B answers.
    let offer = pc_a.create_offer(None).await.unwrap();
    pc_a.set_local_description(offer.clone()).await.unwrap();
    pc_b.set_remote_description(offer).await.unwrap();
    let answer = pc_b.create_answer(None).await.unwrap();
    pc_b.set_local_description(answer.clone()).await.unwrap();
    pc_a.set_remote_description(answer).await.unwrap();

    // Trickle ICE both directions.
    let pc_b_feed = pc_b.clone();
    tokio::spawn(async move {
        while let Some(c) = cand_a_rx.recv().await {
            let _ = pc_b_feed.add_ice_candidate(c).await;
        }
    });
    let pc_a_feed = pc_a.clone();
    tokio::spawn(async move {
        while let Some(c) = cand_b_rx.recv().await {
            let _ = pc_a_feed.add_ice_candidate(c).await;
        }
    });

    wait_connected(&handler_a, &handler_b).await;

    // A encodes a tone and sends it; B must receive the identical payload.
    let mut enc = OpusEncoder::new().unwrap();
    let pcm: Vec<i16> = (0..FRAME_SAMPLES).map(|i| ((i as f64 * 0.04).sin() * 4000.0) as i16).collect();
    let encoded = enc.encode(&pcm).unwrap();
    let mut packetizer = AudioPacketizer::new(0xaaaa_0001, 111);
    track_a.write_rtp(packetizer.packet(&encoded)).await.unwrap();

    let payload = tokio::time::timeout(Duration::from_secs(10), b_packets_rx.recv())
        .await
        .expect("timed out waiting for RTP on the far side")
        .expect("packet channel closed");
    assert_eq!(payload, encoded, "received RTP payload must equal the OPUS frame sent");
}
