#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "[setup] Checking prerequisites..."
command -v cargo >/dev/null 2>&1 || {
  echo "[setup] ERROR: cargo is not installed or not on PATH"
  exit 1
}

echo "[setup] cargo: $(cargo --version)"
echo "[setup] Running cargo check..."
cargo check --quiet
echo "[setup] OK"
