#!/usr/bin/env bash
# Perf capture harness for the Lumen desktop app (Slint).
#
# Runs a system-wide-ish perf record with call-graphs on the lumen process,
# tags every sample with a timestamp (-T), and — when the Slint MCP server is
# up — polls it in parallel to log what screen/state the app is in, so the
# profile can be sliced per phase (idle / voice chat / mic on / denoiser on).
#
# Usage:
#   scripts/perf-capture.sh [DURATION_SECONDS] [OUT_PERF]
#     DURATION_SECONDS  how long to record (default 60)
#     OUT_PERF          perf.data output path (default ./perf.data)
#
# Run as your own user for user-space stacks (paranoid<=2 is enough).
# Run with sudo to also get kernel stacks (paranoid<=1 / -1).
#   sudo scripts/perf-capture.sh 90 /tmp/lumen.perf.data
#
# The app must already be running with SLINT_MCP_PORT set, e.g.:
#   SLINT_MCP_PORT=43210 ./target/release/lumen
# If MCP is not reachable the script still records; phase log is just empty.

set -euo pipefail
cd "$(dirname "$0")/.."

DUR="${1:-60}"
OUT="${2:-./perf.data}"
MCP_PORT="${LUMEN_MCP_PORT:-43210}"
MCP_URL="http://127.0.0.1:${MCP_PORT}/mcp"
PHASE_LOG="${OUT%.data}.phases.log"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# --- locate the lumen process (newest if several) ---
PID="$(pgrep -x lumen | head -1 || true)"
if [ -z "$PID" ]; then
  echo "error: no 'lumen' process running (launch with SLINT_MCP_PORT=$MCP_PORT first)" >&2
  exit 1
fi
echo "recording lumen pid=$PID for ${DUR}s -> $OUT"

# perf may need privileges to attach if run under sudo; keep uid sanity.
PERF="perf"
if [ "$(id -u)" -eq 0 ]; then
  # -F99 user-space default; --all-cpus off; timestamps on; call-graph fp
  PERF="perf"
fi

"$PERF" record -F 99 -g --call-graph fp -T -p "$PID" -o "$OUT" \
  -- sleep "$DUR" &
PERF_PID=$!

# --- parallel MCP phase logger ---
(
  echo "# lumen perf phase log (pid=$PID, started $(date +%T))" > "$PHASE_LOG"
  T0="$(date +%s%N)"
  while kill -0 "$PERF_PID" 2>/dev/null; do
    now="$(date +%s%N)"
    t=$(( (now - T0) / 1000000 ))  # ms since start
    line="t=${t}ms "
    # Query the MCP server for the current window/element text; best effort.
    resp="$(curl -s -m 1 -X POST "$MCP_URL" \
        -H 'Content-Type: application/json' \
        -H 'Accept: application/json, text/event-stream' \
        -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
      | head -c 400 || true)"
    line+="mcp_tools=${resp}"
    echo "$line" >> "$PHASE_LOG"
    sleep 2
  done
) &
MCP_PID=$!

wait "$PERF_PID"
kill "$MCP_PID" 2>/dev/null || true
wait "$MCP_PID" 2>/dev/null || true

echo "done. perf.data=$OUT  phase log=$PHASE_LOG"
echo "slice by phase with:  perf script -i $OUT --time <start>,<end> ..."
