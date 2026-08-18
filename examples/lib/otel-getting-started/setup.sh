#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

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
