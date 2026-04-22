#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

TIMEOUT_SECS="${TIMEOUT_SECS:-45}"
DASHBOARD_PORT="${DASHBOARD_PORT:-3000}"
LOG_FILE="$(mktemp -t ris-live-dashboard.XXXXXX.log)"
SSE_FILE="$(mktemp -t ris-live-sse.XXXXXX.log)"

cleanup() {
  # Kill the entire process group so both timeout and cargo run are terminated
  if [ -n "${BG_PID:-}" ]; then
    kill -- -"$BG_PID" 2>/dev/null || true
    wait "$BG_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

echo "[test-updates] Building example..."
cargo build --quiet

echo "[test-updates] Starting dashboard (port ${DASHBOARD_PORT}) for up to ${TIMEOUT_SECS}s..."
echo "[test-updates] Log: $LOG_FILE"
echo "[test-updates] SSE: $SSE_FILE"

# Start the dashboard in a new process group so we can kill the whole tree
set -m
timeout "${TIMEOUT_SECS}" cargo run >"$LOG_FILE" 2>&1 &
BG_PID=$!
set +m

# Wait for dashboard to start listening
for i in $(seq 1 30); do
  if curl -sf "http://localhost:${DASHBOARD_PORT}/" >/dev/null 2>&1; then
    echo "[test-updates] Dashboard is up (waited ${i}s)"
    break
  fi
  if ! kill -0 "$BG_PID" 2>/dev/null; then
    echo "[test-updates] Process exited before dashboard started"
    cat "$LOG_FILE"
    exit 1
  fi
  sleep 1
done

# Collect SSE events for a few seconds
echo "[test-updates] Collecting SSE events for 15s..."
timeout 15 curl -sN "http://localhost:${DASHBOARD_PORT}/events" >"$SSE_FILE" 2>/dev/null || true

# cleanup trap will stop the process group

echo "[test-updates] Last SSE lines:"
tail -n 20 "$SSE_FILE"

if grep -q 'route-change' "$SSE_FILE"; then
  echo "[test-updates] SUCCESS: SSE stream delivered route-change events"
  exit 0
fi

echo "[test-updates] No route-change events found in SSE stream"
echo "[test-updates] Log output:"
tail -n 30 "$LOG_FILE"
exit 1
