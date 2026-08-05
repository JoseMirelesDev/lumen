//! Voice session orchestration: signaling relay × per-peer WebRTC × audio.
//!
//! The whole stack is native:
//! ```text
//! cpal mic ─▶ opus ─▶ RTP ─▶ TrackLocalStaticRTP ─▶ webrtc-rs ─▶ SRTP ─▶ UDP
//! UDP ─▶ webrtc-rs ─▶ TrackRemote ─▶ jitter buffer ─▶ opus ─▶ cpal out
//! both ends negotiated over the WS relay (docs/protocol.md §1)
//! ```
//!
//! Screen/video is deliberately absent. Every peer here is a single audio
//! track; the negotiation path (`add_track`, `remove_track`, re-offer) is the
//! seam where a video `TrackLocalStaticRTP` slots in later, unchanged.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use rtc::peer_connection::configuration::{RTCConfigurationBuilder, RTCIceServer};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::state::{
    RTCIceGatheringState, RTCPeerConnectionState, RTCSignalingState,
};
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCPeerConnectionIceEvent,
};
use webrtc::rtp_transceiver::RtpSender;

use crate::voice::audio::{rms_level, AudioOutput, JitterBuffer, OpusDecoder, OpusEncoder};
use crate::voice::rtp::AudioPacketizer;
use crate::voice::signaling::{SignalEvent, SignalOut, SignalingClient};

/// STUN/TURN server as the frontend supplies it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

/// Arguments for `voice_join`, as the frontend sends them (camelCase JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceJoinArgs {
    /// HTTP base of the worker, e.g. `https://lumen.example.workers.dev`.
    pub backend_url: String,
    pub token: String,
    pub channel_id: String,
    pub user_id: String,
    #[serde(default)]
    pub ice_servers: Vec<IceServer>,
}

// ---------------------------------------------------------------------------
// VoiceClient: Tauri-managed handle
// ---------------------------------------------------------------------------

pub struct VoiceClient {
    app: AppHandle,
    session: tokio::sync::Mutex<Option<Arc<VoiceSession>>>,
}

impl VoiceClient {
    pub fn new(app: AppHandle) -> Self {
        Self { app, session: tokio::sync::Mutex::new(None) }
    }

    pub async fn join(&self, args: VoiceJoinArgs) -> std::result::Result<(), String> {
        let mut guard = self.session.lock().await;
        if guard.is_some() {
            return Err("already in a voice channel".into());
        }
        let session = VoiceSession::start(self.app.clone(), args).await.map_err(|e| e.to_string())?;
        *guard = Some(session);
        Ok(())
    }

    pub async fn leave(&self) {
        if let Some(session) = self.session.lock().await.take() {
            session.stop().await;
        }
    }

    pub async fn set_muted(&self, muted: bool) {
        if let Some(s) = self.session.lock().await.as_ref() {
            s.muted.store(muted, Ordering::SeqCst);
        }
    }

    pub async fn set_deafened(&self, deafened: bool) {
        if let Some(s) = self.session.lock().await.as_ref() {
            s.deafened.store(deafened, Ordering::SeqCst);
            s.output.set_enabled(!deafened);
        }
    }
}

// ---------------------------------------------------------------------------
// VoiceSession
// ---------------------------------------------------------------------------

struct VoiceSession {
    peers: Arc<Mutex<HashMap<String, Peer>>>,
    output: AudioOutput,
    _mic_stream: cpal::Stream,
    _out_stream: cpal::Stream,
    muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    _encoder: Arc<tokio::sync::Mutex<OpusEncoder>>,
    ice: Vec<RTCIceServer>,
    /// The outbound signaling handle. Dropping it closes the WS → loop exits.
    signal_tx: Mutex<Option<mpsc::UnboundedSender<SignalOut>>>,
    stopping: Arc<AtomicBool>,
}

impl VoiceSession {
    async fn start(app: AppHandle, args: VoiceJoinArgs) -> Result<Arc<Self>> {
        let ws_base = args.backend_url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        let ws_url = format!("{ws_base}/api/ws/{}", args.channel_id);
        let (signal, signal_rx) =
            SignalingClient::connect(&ws_url, &args.token, &args.channel_id, &args.user_id)
                .await
                .context("signaling connect")?;
        let signal_tx = signal.tx.clone();

        let (mic_tx, mut mic_rx) = mpsc::unbounded_channel::<Vec<i16>>();
        let mic_stream = crate::voice::audio::start_capture(mic_tx).context("mic capture")?;
        let output = AudioOutput::new();
        let out_stream = output.start().context("audio output")?;

        let peers = Arc::new(Mutex::new(HashMap::<String, Peer>::new()));
        let muted = Arc::new(AtomicBool::new(false));
        let deafened = Arc::new(AtomicBool::new(false));
        let local_level = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let encoder = Arc::new(tokio::sync::Mutex::new(OpusEncoder::new()?));
        let stopping = Arc::new(AtomicBool::new(false));
        let ice: Vec<RTCIceServer> = args
            .ice_servers
            .iter()
            .map(|s| RTCIceServer {
                urls: s.urls.clone(),
                username: s.username.clone().unwrap_or_default(),
                credential: s.credential.clone().unwrap_or_default(),
            })
            .collect();

        // Send: encode mic frames and fan out to every peer's send worker.
        {
            let peers = peers.clone();
            let muted = muted.clone();
            let encoder = encoder.clone();
            let local_level = local_level.clone();
            let stopping = stopping.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_millis(20));
                loop {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    ticker.tick().await;
                    if muted.load(Ordering::SeqCst) {
                        continue;
                    }
                    let frame = match mic_rx.try_recv() {
                        Ok(f) => f,
                        Err(_) => continue,
                    };
                    local_level.store(rms_level(&frame).to_bits(), Ordering::SeqCst);
                    let encoded = match encoder.lock().await.encode(&frame) {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    for peer in peers.lock().values() {
                        let _ = peer.send_tx.send(encoded.clone());
                    }
                }
            });
        }

        // Levels: emit ~10 Hz so the UI can drive metering.
        {
            let peers = peers.clone();
            let local_level = local_level.clone();
            let app = app.clone();
            let stopping = stopping.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_millis(100));
                loop {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    ticker.tick().await;
                    let local = f32::from_bits(local_level.load(Ordering::SeqCst));
                    let list: Vec<serde_json::Value> = peers
                        .lock()
                        .iter()
                        .map(|(id, p)| {
                            serde_json::json!({
                                "peerId": id,
                                "level": f32::from_bits(p.level.load(Ordering::SeqCst)),
                            })
                        })
                        .collect();
                    let _ = app.emit(
                        "voice://levels",
                        serde_json::json!({ "local": local, "peers": list }),
                    );
                }
            });
        }

        let session = Arc::new(Self {
            peers,
            output,
            _mic_stream: mic_stream,
            _out_stream: out_stream,
            muted,
            deafened,
            _encoder: encoder,
            ice,
            signal_tx: Mutex::new(Some(signal_tx)),
            stopping,
        });

        let loop_session = Arc::clone(&session);
        let app2 = app.clone();
        tokio::spawn(async move { loop_session.run_loop(app2, signal_rx).await; });
        Ok(session)
    }

    /// Drive the signaling loop until the session closes (stop() or socket end).
    async fn run_loop(self: Arc<Self>, app: AppHandle, mut signal_rx: mpsc::UnboundedReceiver<SignalEvent>) {
        loop {
            tokio::select! {
                event = signal_rx.recv() => {
                    let Some(event) = event else { break };
                    self.handle_event(&app, event).await;
                }
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if self.stopping.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        }
        self.teardown(&app).await;
    }

    async fn handle_event(&self, app: &AppHandle, event: SignalEvent) {
        // Re-fetch the current outbound handle each event (it may be dropped).
        let signal_tx = self.signal_tx.lock().clone();
        let Some(signal_tx) = signal_tx else { return };
        match event {
            SignalEvent::Joined { peer_id, peers } => {
                let _ = app.emit(
                    "voice://debug",
                    serde_json::json!({ "message": format!("joined channel as peer {peer_id}") }),
                );
                // Existing peers will offer to us; announce them for the UI.
                for p in peers {
                    let _ = app.emit(
                        "voice://peer-joined",
                        serde_json::json!({ "peerId": p.peer_id, "userId": p.user_id }),
                    );
                }
            }
            SignalEvent::PeerJoined(p) => {
                let _ = app.emit(
                    "voice://peer-joined",
                    serde_json::json!({ "peerId": p.peer_id, "userId": p.user_id }),
                );
                if !self.peers.lock().contains_key(&p.peer_id) {
                    let peer = match self.create_peer(&p.peer_id, &p.user_id, app, &signal_tx).await
                    {
                        Ok(peer) => peer,
                        Err(err) => {
                            let _ = app.emit(
                                "voice://error",
                                serde_json::json!({ "message": err.to_string() }),
                            );
                            return;
                        }
                    };
                    // We are the existing peer: offer to the newcomer.
                    let offer = match peer.pc.create_offer(None).await {
                        Ok(o) => o,
                        Err(err) => {
                            let _ = app.emit(
                                "voice://error",
                                serde_json::json!({ "message": err.to_string() }),
                            );
                            return;
                        }
                    };
                    if peer.pc.set_local_description(offer.clone()).await.is_ok() {
                        let _ = signal_tx.send(SignalOut::Offer {
                            to: p.peer_id.clone(),
                            sdp: offer.sdp,
                        });
                    }
                    self.peers.lock().insert(p.peer_id.clone(), peer);
                }
            }
            SignalEvent::Offer { from, sdp } => self.handle_offer(app, &signal_tx, &from, sdp).await,
            SignalEvent::Answer { from, sdp } => self.handle_answer(app, &from, sdp).await,
            SignalEvent::IceCandidate { from, candidate } => self.handle_ice(app, &from, candidate).await,
            SignalEvent::PeerLeft(peer_id) => self.remove_peer(app, &peer_id).await,
            SignalEvent::Error { code, message } => {
                let _ = app.emit("voice://error", serde_json::json!({ "code": code, "message": message }));
            }
            SignalEvent::Closed => {}
        }
    }

    /// Create a peer connection for `peer_id`. The caller decides who offers.
    async fn create_peer(
        &self,
        peer_id: &str,
        user_id: &str,
        app: &AppHandle,
        signal_tx: &mpsc::UnboundedSender<SignalOut>,
    ) -> Result<Peer> {
        let mut media_engine = MediaEngine::default();
        media_engine.register_default_codecs()?;
        let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;
        let config = RTCConfigurationBuilder::new().with_ice_servers(self.ice.clone()).build();

        let ssrc: u32 = rand::random();
        let track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
            format!("lumen-stream-{peer_id}"),
            format!("lumen-audio-{peer_id}"),
            "microphone".to_string(),
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
                    sdp_fmtp_line: "useinbandfec=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                ..Default::default()
            }],
        )));

        let jb = Arc::new(Mutex::new(JitterBuffer::new(4)));
        let level = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let stop = Arc::new(AtomicBool::new(false));

        let handler = Arc::new(PeerHandler {
            peer_id: peer_id.to_string(),
            signal: signal_tx.clone(),
            app: app.clone(),
            jb: jb.clone(),
            output: self.output.clone(),
            level: level.clone(),
            stop: stop.clone(),
        });

        let runtime = webrtc::runtime::default_runtime().expect("runtime-tokio feature");
        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(config)
                .with_media_engine(media_engine)
                .with_interceptor_registry(registry)
                .with_handler(handler)
                .with_runtime(runtime)
                .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
                .build()
                .await?,
        );
        let sender = pc.add_track(Arc::clone(&track) as Arc<dyn TrackLocal>).await?;

        let (send_tx, send_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        spawn_send_worker(track, sender.clone(), ssrc, send_rx, stop.clone());

        Ok(Peer {
            peer_id: peer_id.to_string(),
            user_id: user_id.to_string(),
            pc,
            sender,
            send_tx,
            jb,
            level,
            stop,
        })
    }

    async fn handle_offer(
        &self,
        app: &AppHandle,
        signal_tx: &mpsc::UnboundedSender<SignalOut>,
        from: &str,
        sdp: String,
    ) {
        // Create the peer if the offer precedes any peer-joined message.
        if !self.peers.lock().contains_key(from) {
            match self.create_peer(from, "", app, signal_tx).await {
                Ok(peer) => {
                    self.peers.lock().insert(from.to_string(), peer);
                }
                Err(err) => {
                    let _ = app.emit("voice://error", serde_json::json!({ "message": err.to_string() }));
                    return;
                }
            }
        }
        let Some(peer) = self.peers.lock().get(from).cloned() else { return };
        let Ok(desc) = RTCSessionDescription::offer(sdp) else { return };
        if peer.pc.set_remote_description(desc).await.is_err() {
            return;
        }
        match peer.pc.create_answer(None).await {
            Ok(answer) => {
                if peer.pc.set_local_description(answer.clone()).await.is_ok() {
                    let _ = signal_tx.send(SignalOut::Answer { to: from.to_string(), sdp: answer.sdp });
                }
            }
            Err(err) => {
                let _ = app.emit("voice://error", serde_json::json!({ "message": err.to_string() }));
            }
        }
    }

    async fn handle_answer(&self, app: &AppHandle, from: &str, sdp: String) {
        let Some(peer) = self.peers.lock().get(from).cloned() else {
            let _ = app.emit(
                "voice://error",
                serde_json::json!({ "message": format!("answer from unknown peer {from}") }),
            );
            return;
        };
        let Ok(desc) = RTCSessionDescription::answer(sdp) else { return };
        let _ = peer.pc.set_remote_description(desc).await;
    }

    async fn handle_ice(&self, app: &AppHandle, from: &str, candidate: serde_json::Value) {
        let Some(peer) = self.peers.lock().get(from).cloned() else {
            return; // ICE may precede SDP in trickle mode; peer is created on offer.
        };
        // JS peers send RTCIceCandidate.toJSON(). candidate + sdpMLineIndex parse;
        // sdpMid/usernameFragment degrade to None, which ICE accepts.
        match serde_json::from_value::<webrtc::peer_connection::RTCIceCandidateInit>(candidate) {
            Ok(init) => {
                if let Err(err) = peer.pc.add_ice_candidate(init).await {
                    let _ = app.emit("voice://error", serde_json::json!({ "message": err.to_string() }));
                }
            }
            Err(_) => {}
        }
    }

    async fn remove_peer(&self, app: &AppHandle, peer_id: &str) {
        // Bind the guard to its own statement so it is dropped before we await.
        let removed = self.peers.lock().remove(peer_id);
        if let Some(peer) = removed {
            peer.stop.store(true, Ordering::SeqCst);
            let _ = peer.pc.close().await;
            let _ = app.emit("voice://peer-left", serde_json::json!({ "peerId": peer_id }));
        }
    }

    async fn teardown(&self, app: &AppHandle) {
        self.stopping.store(true, Ordering::SeqCst);
        let peers: Vec<Peer> = self.peers.lock().drain().map(|(_, p)| p).collect();
        for peer in peers {
            peer.stop.store(true, Ordering::SeqCst);
            let _ = peer.pc.close().await;
        }
        let _ = app.emit("voice://signaling", serde_json::json!({ "state": "closed" }));
    }

    /// End the session. Closes peers and the WS; the loop breaks and tears down.
    async fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        *self.signal_tx.lock() = None; // closes the WS → run_loop exits
        let peers: Vec<Peer> = self.peers.lock().drain().map(|(_, p)| p).collect();
        for peer in peers {
            peer.stop.store(true, Ordering::SeqCst);
            let _ = peer.pc.close().await;
        }
        self.output.set_enabled(false);
    }
}

// ---------------------------------------------------------------------------
// Peer + per-peer tasks
// ---------------------------------------------------------------------------

struct Peer {
    peer_id: String,
    user_id: String,
    pc: Arc<dyn PeerConnection>,
    #[allow(dead_code)]
    sender: Arc<dyn RtpSender>,
    send_tx: mpsc::UnboundedSender<Vec<u8>>,
    #[allow(dead_code)]
    jb: Arc<Mutex<JitterBuffer>>,
    level: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
}

impl Clone for Peer {
    fn clone(&self) -> Self {
        Peer {
            peer_id: self.peer_id.clone(),
            user_id: self.user_id.clone(),
            pc: self.pc.clone(),
            sender: self.sender.clone(),
            send_tx: self.send_tx.clone(),
            jb: self.jb.clone(),
            level: self.level.clone(),
            stop: self.stop.clone(),
        }
    }
}

/// Resolve the payload type negotiated for the sender's (single) codec.
async fn negotiated_payload_type(sender: &Arc<dyn RtpSender>) -> Option<u8> {
    sender.get_parameters().await.ok()?.rtp_parameters.codecs.first().map(|c| c.payload_type)
}

/// Per-peer send worker: packetize encoded frames and write them to the track.
fn spawn_send_worker(
    track: Arc<TrackLocalStaticRTP>,
    sender: Arc<dyn RtpSender>,
    ssrc: u32,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        let mut packetizer = AudioPacketizer::new(ssrc, 111);
        let mut pt: Option<u8> = None;
        while let Some(frame) = rx.recv().await {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if pt.is_none() {
                if let Some(p) = negotiated_payload_type(&sender).await {
                    packetizer.payload_type = p;
                    pt = Some(p);
                } else {
                    continue; // not negotiated yet — wait for a later frame
                }
            }
            let pkt = packetizer.packet(&frame);
            let _ = track.write_rtp(pkt).await; // errors while unconnected are dropped
        }
    });
}

// ---------------------------------------------------------------------------
// Per-peer event handler
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct PeerHandler {
    peer_id: String,
    signal: mpsc::UnboundedSender<SignalOut>,
    app: AppHandle,
    jb: Arc<Mutex<JitterBuffer>>,
    output: AudioOutput,
    level: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
}

fn state_str(s: RTCPeerConnectionState) -> &'static str {
    match s {
        RTCPeerConnectionState::New => "new",
        RTCPeerConnectionState::Connecting => "connecting",
        RTCPeerConnectionState::Connected => "connected",
        RTCPeerConnectionState::Disconnected => "disconnected",
        RTCPeerConnectionState::Failed => "failed",
        RTCPeerConnectionState::Closed => "closed",
        _ => "unknown",
    }
}

#[async_trait]
impl PeerConnectionEventHandler for PeerHandler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            // Match the JS RTCIceCandidate.toJSON() shape so JS peers parse it.
            let _ = self.signal.send(SignalOut::IceCandidate {
                to: self.peer_id.clone(),
                candidate: serde_json::json!({
                    "candidate": init.candidate,
                    "sdpMid": init.sdp_mid,
                    "sdpMLineIndex": init.sdp_mline_index,
                    "usernameFragment": init.username_fragment,
                }),
            });
        }
    }

    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.app.emit(
                "voice://debug",
                serde_json::json!({ "peerId": self.peer_id, "message": "ice gathering complete" }),
            );
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.app.emit(
            "voice://state",
            serde_json::json!({ "peerId": self.peer_id, "state": state_str(state) }),
        );
    }

    async fn on_signaling_state_change(&self, _state: RTCSignalingState) {}

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        // Receiver: push inbound RTP into the shared jitter buffer.
        let recv_jb = self.jb.clone();
        let stop_recv = self.stop.clone();
        tokio::spawn(async move {
            while let Some(event) = track.poll().await {
                if stop_recv.load(Ordering::SeqCst) {
                    break;
                }
                match event {
                    TrackRemoteEvent::OnRtpPacket(pkt) => {
                        let ts = pkt.header.timestamp;
                        let seq = pkt.header.sequence_number;
                        recv_jb.lock().push(seq, ts, pkt.payload.to_vec());
                    }
                    TrackRemoteEvent::OnEnded | TrackRemoteEvent::OnEnding | TrackRemoteEvent::OnError => {
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Player: playout clock at 20 ms cadence; decode + push to output.
        let play_jb = self.jb.clone();
        let output = self.output.clone();
        let level = self.level.clone();
        let stop_play = self.stop.clone();
        tokio::spawn(async move {
            let mut decoder = match OpusDecoder::new() {
                Ok(d) => d,
                Err(_) => return,
            };
            let mut ticker = tokio::time::interval(Duration::from_millis(20));
            loop {
                if stop_play.load(Ordering::SeqCst) {
                    break;
                }
                ticker.tick().await;
                let frame = play_jb.lock().pop();
                match frame {
                    Some(Some((_, payload))) => {
                        if let Ok(pcm) = decoder.decode(Some(payload.as_slice())) {
                            level.store(rms_level(&pcm).to_bits(), Ordering::SeqCst);
                            output.push(&pcm);
                        }
                    }
                    // Lost frame: run PLC to avoid a gap.
                    Some(None) => {
                        if let Ok(pcm) = decoder.decode(None) {
                            output.push(&pcm);
                        }
                    }
                    None => {}
                }
            }
        });
    }
}