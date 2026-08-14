#!/usr/bin/env bash
# Launches the SECOND Lumen client (receive-only, no mic) inside the lumen2
# netns, so its RTP traverses the veth + tc netem (real latency/loss).
# Run with sudo. Reads its token from scripts/.token2 (no long pastes).
set -euo pipefail
cd "$(dirname "$0")/.."
TOKEN2=$(cat scripts/.token2)
exec sudo ip netns exec lumen2 env LUMEN_VOICE_DIAG=1 timeout 45 \
  ./target/debug/p2p_diag --single --no-mic --no-output --stun-only \
  "https://lumen-backend.renymirelesd.workers.dev" \
  "$TOKEN2" "9385c946-dc4b-457d-9231-7014a888a25f" \
  "152108c5-e37a-4d0f-910d-e28f43ee1539"
