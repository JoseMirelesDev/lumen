#!/usr/bin/env bash
# Synchronized dual-client latency test (runs as root via the sudoers rule).
#
#   * client1 (reny, host): REAL USB mic (UACDemoV1.0 — 40 ms ALSA bursts,
#     which is what exercises the threshold-gated shed fix), STUN-only,
#     --flip-model-at flips the denoiser mid-call to prove live switching.
#     Runs as the real user (runuser) because root has no access to the
#     user's PipeWire/PulseAudio session.
#   * client2 (diagtest2, netns lumen2): receive-only, no mic, no output;
#     its RTP traverses the veth + tc netem (real latency/loss).
#
#   sudo bash scripts/lumen-dual.sh

set -euo pipefail
cd "$(dirname "$0")/.."

USER_NAME="${SUDO_USER:-reny}"
USER_HOME="$(getent passwd "$USER_NAME" | cut -d: -f6)"
USER_UID="$(id -u "$USER_NAME")"
TOKEN1=$(python3 -c "import json;print(json.load(open('$USER_HOME/.config/lumen/settings.json'))['token'])")
TOKEN2=$(cat scripts/.token2)
BACKEND="https://lumen-backend.renymirelesd.workers.dev"
CH="152108c5-e37a-4d0f-910d-e28f43ee1539"
U1="b258651d-47b1-40ba-9a36-f8b46b335cbe"
U2="9385c946-dc4b-457d-9231-7014a888a25f"
HOST_IP="$(ip -4 -o addr show scope global | awk '{print $4}' | cut -d/ -f1 | head -1)"

# The netns may be gone (teardown/reboot). Recreate idempotently — the setup
# script cleans previous state first, so calling it is always safe.
if ! ip netns list | grep -q '^lumen2'; then
  echo "[*] netns lumen2 no existe — recreando..."
  bash scripts/lumen-netns-setup.sh
fi

echo "[*] lanzando cliente1 ($USER_NAME, MIC REAL, host $HOST_IP, flip model@12s)..."
rm -f /tmp/lumen-voice-diag-*.jsonl /tmp/c1.log /tmp/c2.log 2>/dev/null || true
runuser -u "$USER_NAME" -- env \
  XDG_RUNTIME_DIR="/run/user/$USER_UID" \
  LUMEN_VOICE_DIAG=1 LUMEN_BIND_ADDR="$HOST_IP" \
  timeout 60 ./target/debug/p2p_diag --single --no-output --stun-only \
  --flip-model-at=12 "$BACKEND" "$TOKEN1" "$U1" "$CH" >/tmp/c1.log 2>&1 &
C1=$!

echo "[*] esperando 6 s a que cliente1 se una..."
sleep 6

echo "[*] lanzando cliente2 (diagtest2, netns lumen2, receive-only)..."
timeout 50 ip netns exec lumen2 env LUMEN_VOICE_DIAG=1 LUMEN_BIND_ADDR=10.77.0.2 \
  ./target/debug/p2p_diag --single --no-mic --no-output --stun-only \
  "$BACKEND" "$TOKEN2" "$U2" "$CH" >/tmp/c2.log 2>&1 || true

echo "[*] cliente2 terminó; esperando a cliente1..."
wait $C1 2>/dev/null || true

echo ""
echo "=== resumen ==="
echo "  c1 joined: $(grep -c 'joined' /tmp/c1.log 2>/dev/null || echo 0)"
echo "  c1 peers: $(grep -oE 'peers: \[[^]]*\]' /tmp/c1.log 2>/dev/null | sort | uniq -c | tail -1)"
echo "  diag files:"
ls -la /tmp/lumen-voice-diag-*.jsonl 2>/dev/null
