#!/bin/bash
# Start the bundled Hello World agent, then this example.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if ! command -v python3 >/dev/null 2>&1; then
    echo "python3 is required for the bundled Hello World agent."
    exit 1
fi

if lsof -ti:9000 >/dev/null 2>&1; then
    echo "Port 9000 is already in use. Free it and retry."
    exit 1
fi

AGENT_PID=""
cleanup() {
    if [[ -n "${AGENT_PID}" ]] && kill -0 "${AGENT_PID}" 2>/dev/null; then
        kill "${AGENT_PID}" 2>/dev/null || true
        wait "${AGENT_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

if lsof -ti:9999 >/dev/null 2>&1; then
    echo "Port 9999 is already in use. Free it and retry."
    exit 1
fi

python3 "${SCRIPT_DIR}/agent.py" &
AGENT_PID=$!

for _ in $(seq 1 50); do
    if python3 -c "import socket; socket.create_connection(('127.0.0.1', 9999), 0.2).close()" 2>/dev/null; then
        break
    fi
    if ! kill -0 "${AGENT_PID}" 2>/dev/null; then
        echo "Hello World agent exited before binding port 9999."
        exit 1
    fi
    sleep 0.1
done

if ! python3 -c "import socket; socket.create_connection(('127.0.0.1', 9999), 0.2).close()" 2>/dev/null; then
    echo "Hello World agent failed to bind port 9999."
    exit 1
fi

cargo run
