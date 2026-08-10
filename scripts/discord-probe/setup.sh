#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# Discord/Krisp A/B probe — virtual audio devices
#
# Creates two virtual devices so a synthetic noisy WAV can be injected into
# Discord's mic and Discord's (Krisp-processed) output captured back:
#   discord_mic_out  : null sink  — feed the noisy WAV here; its monitor is
#                                    the "microphone" Discord reads.
#   discord_spk      : null sink  — Discord plays its output here; we record
#                                    discord_spk.monitor to capture it.
#
# Requirement: pipewire-pulse must be able to load module-null-sink. The most
# reliable way is a PulseAudio config that pipewire-pulse reads at startup.
# The `pw-cli create-node` route creates the nodes but they are NOT adopted by
# WirePlumber (stay "suspended", never linked) — do not use it.
#
# Usage:
#   1. Run this script (once). It writes ~/.config/pulse/discord-probe.pa and
#      tells you how to apply it (pipewire-pulse restart OR `pactl` if you
#      install pulseaudio-utils).
#   2. In Discord: Voice & Video -> Input device = "discord_mic_out.monitor",
#      Output device = "discord_spk", Noise Suppression = "Krisp", disable
#      echo cancellation + AGC (we want raw Krisp only).
#   3. Feed the WAV:    pw-play --target discord_mic_out samples/X.wav  (loop it)
#      Capture output:  pw-record --target discord_spk.monitor out.wav
#   4. Compare with scripts/discord-probe/compare.py
# ---------------------------------------------------------------------------
set -euo pipefail

PA_FILE="$HOME/.config/pulse/discord-probe.pa"
mkdir -p "$HOME/.config/pulse"

cat > "$PA_FILE" <<'EOF'
load-module module-null-sink sink_name=discord_mic_out
load-module module-null-sink sink_name=discord_spk
EOF

echo "Wrote $PA_FILE"
echo
if command -v pactl >/dev/null 2>&1; then
  echo "pactl found — applying now:"
  pactl load-module module-null-sink sink_name=discord_mic_out
  pactl load-module module-null-sink sink_name=discord_spk
  echo "Devices created."
else
  echo "pactl not installed. To apply, either:"
  echo "  sudo apt install pulseaudio-utils   # then: pactl load-module ... (or re-run this script)"
  echo "  OR restart pipewire-pulse (it reads $PA_FILE on start):"
  echo "      systemctl --user restart pipewire-pulse"
  echo "  (a brief audio glitch on restart is normal)"
fi
echo
echo "Verify:"
echo "  pactl list short sinks | grep discord       # discord_mic_out, discord_spk"
echo "  pactl list short sources | grep discord     # ..._monitor sources"
