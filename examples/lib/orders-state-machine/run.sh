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
        echo "Postgres is ready."
        break
    fi
    sleep 1
done

echo ""
echo "Starting Drasi + dashboard (http://localhost:3000)..."
echo "Press Ctrl+C to stop the app; run 'docker compose down -v' to remove the database."
echo ""

cargo run
