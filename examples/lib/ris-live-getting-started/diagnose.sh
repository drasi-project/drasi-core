#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "[diagnose] Directory: $SCRIPT_DIR"
echo "[diagnose] Rust toolchain:"
rustc --version
cargo --version
echo

echo "[diagnose] Effective configuration:"
echo "  DASHBOARD_PORT=${DASHBOARD_PORT:-3000}"
echo "  RIS_HOST=${RIS_HOST:-rrc00}"
echo "  RIS_MESSAGE_TYPE=${RIS_MESSAGE_TYPE:-UPDATE}"
echo "  RIS_PREFIX=${RIS_PREFIX:-<none>}"
echo "  RIS_WS_URL=${RIS_WS_URL:-wss://ris-live.ripe.net/v1/ws/}"
echo "  RIS_INCLUDE_PEER_STATE=${RIS_INCLUDE_PEER_STATE:-true}"
echo "  RIS_RECONNECT_DELAY_SECS=${RIS_RECONNECT_DELAY_SECS:-5}"
echo "  RUST_LOG=${RUST_LOG:-info}"
echo

echo "[diagnose] Running cargo check..."
cargo check --quiet
echo "[diagnose] Running cargo test --no-run..."
cargo test --quiet --no-run
echo "[diagnose] OK"
