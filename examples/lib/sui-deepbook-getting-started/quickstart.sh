#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "${SCRIPT_DIR}"

./setup.sh

echo "Building example..."
cargo build

echo
echo "Starting Sui DeepBook getting-started example..."
echo "Press Ctrl+C to stop."
echo

cargo run
