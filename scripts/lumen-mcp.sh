#!/usr/bin/env bash
# Run the Lumen Slint client with the embedded MCP server enabled.
#
# The Slint testing backend ships an embedded MCP (Model Context Protocol)
# server: `--features slint/mcp` compiles it and `SLINT_MCP_PORT` makes it
# listen. `SLINT_EMIT_DEBUG_INFO=1` embeds element metadata so introspection
# (and the MCP tools) can address UI elements.
#
# Usage:
#   scripts/lumen-mcp.sh [port]          # default 8080
#   SLINT_BACKEND=headless scripts/lumen-mcp.sh  # windowless (CI / sandbox)
#
# Connect an MCP client to http://127.0.0.1:8080/mcp
# (type: streamable-http) or probe with curl:
#   curl -s -X POST http://127.0.0.1:8080/mcp -H "Content-Type: application/json" \
#     -H "Accept: application/json, text/event-stream" \
#     -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"curl","version":"1"}}}'
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${1:-8080}"
export SLINT_EMIT_DEBUG_INFO=1
export SLINT_MCP_PORT="$PORT"

cargo run -p lumen-desktop --features slint/mcp
