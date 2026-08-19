#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

export OTEL_ENDPOINT="${OTEL_ENDPOINT:-http://127.0.0.1:4317}"

echo "Sending telemetry to Collector at ${OTEL_ENDPOINT}..."
echo "Sending CREATE gauge 920..."
cargo run --quiet --bin send-otlp -- 920
sleep 1
echo "Sending UPDATE gauge 700..."
cargo run --quiet --bin send-otlp -- 700
echo "Sending CLIENT span..."
cargo run --quiet --bin send-otlp -- 700 --span
echo "✓ Client harness sent CREATE, UPDATE, and topology events through the Collector"
echo "  Watch the example process for Added/Updated log lines."
