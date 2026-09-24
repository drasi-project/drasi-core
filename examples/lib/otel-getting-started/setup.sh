#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if ! command -v docker >/dev/null 2>&1; then
    echo "Docker is required to run the OpenTelemetry Collector."
    exit 1
fi

echo "Starting OpenTelemetry Collector..."
docker compose up -d

echo "Waiting for Collector health (60s timeout)..."
for i in $(seq 1 60); do
    if curl -fsS "http://127.0.0.1:13133/" >/dev/null 2>&1; then
        echo "✓ Collector is ready on 127.0.0.1:4317"
        break
    fi
    if [ "$i" -eq 60 ]; then
        echo "✗ Collector did not become healthy"
        docker logs drasi-otel-collector --tail 80 || true
        exit 1
    fi
    if [ $((i % 10)) -eq 0 ]; then
        echo "  Still waiting... ($i/60)"
    fi
    sleep 1
done

echo "Building otel-getting-started..."
for i in {1..60}; do
    if cargo build --bins; then
        echo "✓ Build complete"
        exit 0
    fi
    if [ "$i" -eq 60 ]; then
        echo "✗ Failed to build after 60 attempts"
        exit 1
    fi
    echo "  Still waiting... ($i/60)"
    sleep 2
done
