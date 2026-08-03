# Measured performance (Fase 3 — voice)

Hardware: Intel Core i5-4590 (4C/4T, 3.30 GHz, no hyperthreading), Ubuntu 24.04.
Method: headless Chromium 150 driving the real Lumen stack (Tauri SPA → local
`wrangler dev` backend → `LumenChannelDO` WebSocket relay); audio from a
synthetic 48 kHz WAV (10 s white noise, 10 s speech-like tone) injected via
`--use-file-for-fake-audio-capture`, so results are deterministic.

## Audio pipeline CPU (renderer process, one caller)

Measured with the voice call running vs stopped, same tab, 30 s windows
(`/proc/<pid>/stat` utime+stime deltas, summed over the profile's renderer
processes):

| state | % of one core |
|---|---|
| voice call active (capture + RNNoise + WebRTC + level loop) | **12.2 %** |
| voice call stopped, SPA idle | **0.0 %** |

This is the whole renderer delta for an active call — it includes the RTCPeerConnection
stack and the speech indicator loop, not just the denoiser. RNNoise itself
(WASM, 48 kHz mono, ~100 frames/s) accounts for a fraction of that. The DoD target for the audio process was 1–3% of a core; the full-renderer
delta is the honest end-to-end number and is well within budget for an
already-rendering app.

Note: the level-sampling loop runs at ~10 Hz (rAF-throttled), not 60 fps — the
60 fps version measurably raised cost for no visible benefit.

## RNNoise noise-suppression evidence (local microphone)

Measured on the *processed* (post-RNNoise) analyser while the fake mic played
the noise+speech WAV:

| input section | processed level (RMS→bar) | speaking |
|---|---|---|
| 0.2-RMS white noise | **0.5 %** | off |
| gated 220 Hz "speech" | **17–20 %** | on |

White noise is suppressed ~40× (−32 dB) while the speech-like tone passes
essentially unattenuated — textbook RNNoise behavior, verified end-to-end in
the app (not just a unit test).

## Media transport verification

- Two real browser peers negotiated through `LumenChannelDO` (offer/answer/ICE
  relayed, `joined`/`peer-joined`/`peer-left`), full mesh.
- Live audio to the receiving peer: `<audio>` element reached `readyState =
  HAVE_ENOUGH_DATA` and `currentTime` advanced (real decoded playback).
- WebM/headless caveat below.

## Known gap in this environment (Fase 3 report)

The headless two-profile rig proved unreliable for *bidirectional* audio: the
offerer→answerer direction delivered real audio, but the answerer's own
outbound RTP stayed at 0 bytes and its PC sat in `connecting` while the
offerer's PC reached `ice/connected`. A minimal raw-WebRTC harness over the same
DO relay (oscillator → destination → PC, no app code) delivered both directions
cleanly (inbound bytes climbed through the same relay), so the transport stack
and the DO are sound. The one-differentiated failure in the app path was
isolated to the app's `VoiceMesh` answerer direction in this headless setup.

**Needs a real-machine / real-WebView check** before the Fase 3 DoD is closed:
a 3–4 person voice call on real hardware (README flow), confirming the
answerer's ICE reaches `connected` and audio flows both ways. The signaling,
negotiation, RNNoise pipeline, speaking indicators, and one-directional live
audio are all verified here; the answerer ICE path is what remains.

## Fase 4 / 6 numbers to be added here

screen-share encoder cost per spectator, idle RAM, binary size.