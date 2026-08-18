#!/usr/bin/env bash
set -euo pipefail

BIND="${OTEL_GRPC_BIND:-127.0.0.1:4317}"
HOST="${BIND%:*}"
PORT="${BIND##*:}"

echo "Checking OTLP/gRPC ${HOST}:${PORT}..."
if nc -z "$HOST" "$PORT" 2>/dev/null; then
    echo "✓ Port ${PORT} is accepting connections"
else
    echo "✗ Nothing is listening on ${BIND}"
    echo "  Start the example with: ./quickstart.sh"
    exit 1
fi
