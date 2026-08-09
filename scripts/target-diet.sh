#!/usr/bin/env bash
# Disk diet for target/ — run when target/ grows.
#
# 1. Deletes build artifacts whose stem is neither a Cargo.lock package nor a
#    workspace source (orphans: scratch_* tests deleted long ago, etc.).
# 2. Keeps only the newest generation per surviving stem (each rebuild of a
#    test/probe leaves a fresh binary; older ones are dead weight).
# 3. Drops the incremental cache (regenerates automatically).
#
# Usage: scripts/target-diet.sh [--dry-run]

set -euo pipefail
cd "$(dirname "$0")/.."
DRY="${1:-}"
freed=0

# Normalized package names from Cargo.lock ("name = "x"" -> x with -_ stripped)
LOCK_NAMES="$(awk -F'"' '/^name = /{gsub(/[-_]/, "", $2); print $2}' Cargo.lock | sort -u)"

has_local_source() { # $1 = stem (with -_ stripped)
  local stem="$1"
  find crates apps vendor -type f \( \
    -path "*/src/$stem.rs" -o -path "*/tests/$stem.rs" \
    -o -path "*/examples/$stem.rs" -o -path "*/benches/$stem.rs" \
    -o -path "*/src/bin/$stem.rs" \) -print -quit 2>/dev/null | grep -q .
}

prune() { # $1 = deps dir
  local dir="$1"; [ -d "$dir" ] || return 0
  # --- pass 1: orphans (no Cargo.lock package, no local source) ---
  for f in "$dir"/lib*-*.* "$dir"/[!l]*.*; do
    [ -f "$f" ] || continue
    local base stem norm
    base="$(basename "$f")"
    stem="${base%%-*}"
    stem="${stem#lib}"
    norm="$(printf '%s' "$stem" | tr -d '_-')"
    if ! grep -qx "$norm" <<<"$LOCK_NAMES" && ! has_local_source "$norm"; then
      local sz; sz="$(stat -c%s "$f" 2>/dev/null || echo 0)"
      if [ "$DRY" = "--dry-run" ]; then echo "would remove (orphan): $f ($((sz/1048576)) MB)"; else rm -f "$f"; fi
      freed=$((freed + sz))
    fi
  done
  # --- pass 2: keep newest per surviving stem ---
  for f in "$dir"/lib*-*.* "$dir"/[!l]*.*; do
    [ -f "$f" ] || continue
    local base stem
    base="$(basename "$f")"
    stem="${base%-*}"          # strip the trailing -hash(.ext)
    [ -n "$stem" ] || continue
    local newest
    newest="$(ls -t "$dir"/"${stem}"* 2>/dev/null | head -1 || true)"
    [ -n "$newest" ] || continue
    [ "$f" = "$newest" ] && continue
    local sz; sz="$(stat -c%s "$f" 2>/dev/null || echo 0)"
    if [ "$DRY" = "--dry-run" ]; then echo "would remove (stale): $f ($((sz/1048576)) MB)"; else rm -f "$f"; fi
    freed=$((freed + sz))
  done
}

prune target/debug/deps
if [ "$DRY" = "--dry-run" ]; then
  echo "would free $((freed/1048576)) MB"
else
  rm -rf target/debug/incremental
  echo "freed $((freed/1048576)) MB (plus incremental)"
  du -sh target 2>/dev/null || true
fi
