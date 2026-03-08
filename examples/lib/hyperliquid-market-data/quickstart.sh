#!/bin/bash
set -e

cd "$(dirname "$0")"

./setup.sh

echo "Starting Hyperliquid market data example..."
RUST_LOG=info cargo run
