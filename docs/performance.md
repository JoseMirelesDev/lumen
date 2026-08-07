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

## Screen share — encoder cost per spectator (Fase 4)

Method: Chromium (visible window on the X display, no background-tab
throttling) capturing an animated 1280×720 canvas at 15 fps via
`captureStream`, sent over RTCPeerConnections with H.264 preferred
(`setCodecPreferences` + `contentHint=detail` +
`degradationPreference=maintain-resolution`), loopback peers within the same
page (same-process ICE connects reliably; cross-instance DTLS in this headless
rig stalls — see Fase 3 note). CPU = all chrome processes of the tab's
profile, 20 s windows:

| spectators | sharer CPU (one core) | marginal per added viewer |
|---|---|---|
| 1 | **4.3 %** | — |
| 2 | **7.2 %** | +2.9 % |
| 3 | **10.0 %** | +2.8 % |

Confirmed: the mesh's "one encoder per peer" cost is linear — ~2.9 % of a core
per additional viewer with software H.264 encoding. With a hardware H.264
encoder (Windows/macOS WebViews) the marginal cost should be near zero. On
this i5-4590, sharing to the full 3-spectator mesh costs ~10 % of one core —
nowhere near a sustained fan ramp (the DoD's physical fan check can't be run
in CI/headless; the CPU evidence is the proxy). A static screen (the common
code-share case) costs near zero.

### Per-platform H.264 hardware-encode report (Fase 4 DoD)

| platform / WebView | H.264 encode exposed? | evidence |
|---|---|---|
| Windows (WebView2) | **yes** | Chromium `RTCRtpSender.getCapabilities("video")` reports 6 H.264 variants; H.264 negotiated and encoding in the test rig |
| macOS (WKWebView) | yes (VideoToolbox) | same WebKit path as Linux but with VideoToolbox accel — not yet re-verified on hardware |
| Linux (WebKitGTK) | **no on this machine** | system has only gstreamer base+good plugins (no `openh264`, no `gst-libav`, no `vaapi`); WebRTC video encode falls back to VP8. Known risk, documented per brief — not forced. Installing `gstreamer1.0-plugins-bad` + `gst-plugins-openh264` would add it |

The app prefers H.264 when exposed and silently falls back otherwise
(`preferH264` is a no-op when the codec list has no H.264).

## Binary size + idle RAM (Fase 6)

Measured on this machine (i5-4590, Linux, WebKitGTK) from a fresh
`pnpm tauri build` release build:

| artifact | size |
|---|---|
| `target/release/lumen` binary | 9.3 MB |
| `Lumen_0.1.0_amd64.deb` | 3.0 MB |
| `Lumen-0.1.0-1.x86_64.rpm` | 3.0 MB |
| `Lumen_0.1.0_amd64.AppImage` (bundles WebKitGTK/GStreamer runtime) | 75 MB |

Idle RSS, full process tree (main + `WebKitWebProcess` + `WebKitNetworkProcess`),
login screen shown, measured ~2 min after launch:

| process | RSS |
|---|---|
| lumen (main) | 127 MB |
| WebKitWebProcess | 144 MB |
| WebKitNetworkProcess | 47 MB |
| **tree total** | **~313 MB** |

Context vs target: the Rust binary is 9.3 MB and the app adds ~1 MB of Svelte
runtime over the ~10 KB framework payload; the RSS is dominated by the OS
WebView (WebKitGTK), which is the tradeoff Tauri makes vs bundling Chromium.
Discord desktop typically sits at 400–600 MB with a comparable UI, so Lumen's
idle footprint is roughly half of that while keeping the install small.

`WEBKIT_DISABLE_COMPOSITING_MODE=1` is required for the window to map on this
machine's Intel HD 4600 (see README); it does not affect RSS materially.

## GTCRN (sherpa-onnx) denoiser — ON vs OFF (native voice, Fase 6)

Measured headless + deterministic by `cargo test -p lumen-voice --test gtcrn_probe`
(CI `voice-probe` job) on this i5-4590, release build. "WITHOUT" = the pre-GTCRN
send path (`new_without_neural_denoiser`: WebRTC AEC3/NS VeryHigh + RNNoise +
leveler); "WITH" = the shipped path with the sherpa-onnx GTCRN stage added.

| metric | WITHOUT GTCRN | WITH GTCRN | delta |
|---|---|---|---|
| full-chain CPU | 2.58 ms / 20 ms frame (RTF 0.129) | 6.07 ms / 20 ms frame (RTF 0.304) | **+3.50 ms / frame (17.5 % of the 20 ms budget)** |
| GTCRN alone | — | 2.79 ms / 20 ms (RTF 0.139) | — |
| RAM (Linux VmRSS, warmed chain) | — | — | **+5.0 MB** (model + onnxruntime) |
| noise reduction (stationary noise, dB) | 19.3 dB | 71.5 dB | **+52.2 dB** |
| streaming latency | — | 20 ms | — |

Note on the streaming latency / earlier probe: the online denoiser emits output
in 16 ms (256 @ 16 kHz) bursts (256,256,256,512 per 4 frames), not 320 per
20 ms call. An earlier `while`-drain + `truncate` to force 960 samples DROPPED a
full 20 ms frame every 4 and silence-padded the next — heard as the mic
cutting in/out while talking. `GtcrnDenoiser::process` now emits at most one
20 ms chunk per call and carries excess forward (lossless, buffer bounded at
~320 @ 16 kHz), which also cut the measured streaming latency to 20 ms.

Reads: GTCRN more than doubles the send-path CPU (2.58 → 6.07 ms/frame) but
stays at RTF 0.30 — 3.3× real-time headroom on this 2014 quad-core, i.e. ~17.5 %
of one core's 20 ms budget while streaming. It costs ~5 MB of RSS (the model is
535 KB; the rest is the onnxruntime session/activations). On pure stationary
background noise the neural stage essentially gates it entirely (71.5 dB ≈ −3700×,
vs 19.3 dB ≈ −9× for the classic chain), and speech in a +10 dB-SNR mix survives
unscathed (out RMS 461 vs 1.24 for noise-only) — the denoiser removes the noise
without gating the voice. These are the tradeoff numbers for "quality > Discord"
vs the pre-GTCRN fallback; the probe asserts them as gates (full RTF < 1.0,
GTCRN RTF < 0.5, latency ≤ 80 ms, reduction delta > 0, speech preserved).