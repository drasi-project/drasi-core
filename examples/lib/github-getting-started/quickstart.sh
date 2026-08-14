#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"

./setup.sh
echo "Starting GitHub source getting-started example..."
RUST_LOG="${RUST_LOG:-info}" cargo run
