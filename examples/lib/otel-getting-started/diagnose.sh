#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

fail=0

check_port() {
    local name="$1"
    local host="$2"
    local port="$3"
    echo "Checking ${name} ${host}:${port}..."
    if nc -z "$host" "$port" 2>/dev/null; then
        echo "✓ ${name} is accepting connections"
    else
        echo "✗ Nothing is listening on ${host}:${port}"
        fail=1
    fi
}

if docker compose ps >/dev/null 2>&1; then
    docker compose ps
else
    echo "✗ docker compose is not available"
    fail=1
fi

if curl -fsS "http://127.0.0.1:13133/" >/dev/null 2>&1; then
    echo "✓ Collector health endpoint is up"
else
    echo "✗ Collector health endpoint http://127.0.0.1:13133/ is down"
    echo "  Start it with: ./setup.sh"
    fail=1
fi

check_port "Collector OTLP/gRPC" "127.0.0.1" "4317"
check_port "Drasi OTLP/gRPC" "127.0.0.1" "14317"

if [ "$fail" -ne 0 ]; then
    echo "  Start the example with: ./quickstart.sh"
    exit 1
fi
