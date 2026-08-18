#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

./setup.sh
export RUST_LOG="${RUST_LOG:-info}"
export OTEL_GRPC_BIND="${OTEL_GRPC_BIND:-127.0.0.1:4317}"
exec cargo run --bin otel-getting-started
