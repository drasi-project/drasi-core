#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

./setup.sh

DASHBOARD_PORT="${DASHBOARD_PORT:-3000}"

echo "[quickstart] Starting RIS Live dashboard..."
echo "[quickstart] Dashboard URL: http://localhost:${DASHBOARD_PORT}"
echo "[quickstart] RIS_HOST=${RIS_HOST:-rrc00}"
echo "[quickstart] RIS_MESSAGE_TYPE=${RIS_MESSAGE_TYPE:-UPDATE}"
echo "[quickstart] RIS_PREFIX=${RIS_PREFIX:-<none>}"
echo "[quickstart] RIS_WS_URL=${RIS_WS_URL:-wss://ris-live.ripe.net/v1/ws/}"
echo "[quickstart] Press Ctrl+C to stop"

cargo run
