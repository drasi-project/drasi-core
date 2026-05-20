#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT_DIR"

./setup.sh

export ORACLE_PORT="${ORACLE_PORT:-1522}"

if [ -z "${DYLD_LIBRARY_PATH:-}" ] && [ -z "${LD_LIBRARY_PATH:-}" ]; then
  echo "Oracle client runtime is not configured. Set DYLD_LIBRARY_PATH or LD_LIBRARY_PATH before running the example."
  exit 1
fi

cargo run --manifest-path "$ROOT_DIR/Cargo.toml"
