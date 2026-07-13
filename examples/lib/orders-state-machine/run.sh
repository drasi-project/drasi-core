#!/bin/bash
# Copyright 2025 The Drasi Authors.
#
# Run the Drasi Orders State Machine example.
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

echo "Starting Postgres (logical replication enabled)..."
docker compose up -d

echo "Waiting for Postgres to become healthy..."
for i in $(seq 1 30); do
    status="$(docker inspect -f '{{.State.Health.Status}}' orders-state-machine-db 2>/dev/null || echo starting)"
    if [ "$status" = "healthy" ]; then
        echo "Postgres container is healthy."
        break
    fi
    sleep 1
done

# The container healthcheck only proves Postgres is up *inside* the container. The
# app connects from the host, so verify the published host port (5442) is actually
# reachable. If it is not, the port was likely not published (e.g. something else
# is already bound to it) and the app would fail with "connection refused".
HOST_PORT=5442
echo "Verifying Postgres is reachable on 127.0.0.1:${HOST_PORT}..."
reachable=0
for i in $(seq 1 20); do
    if (exec 3<>/dev/tcp/127.0.0.1/${HOST_PORT}) 2>/dev/null; then
        exec 3>&- 3<&-
        reachable=1
        echo "Postgres is reachable on 127.0.0.1:${HOST_PORT}."
        break
    fi
    sleep 1
done
if [ "$reachable" != "1" ]; then
    echo "ERROR: Postgres is not reachable on 127.0.0.1:${HOST_PORT}." >&2
    echo "       The container is running but its port is not published to the host." >&2
    echo "       Another process/container may already be using host port ${HOST_PORT}." >&2
    echo "       Check with: docker ps  |  lsof -nP -iTCP:${HOST_PORT}" >&2
    exit 1
fi

echo ""
echo "Starting Drasi + dashboard (http://localhost:3002)..."
echo "Press Ctrl+C to stop the app; run 'docker compose down -v' to remove the database."
echo ""

cargo run
