#!/usr/bin/env bash
set -euo pipefail

echo "Starting Grafana + Loki stack with docker compose..."
docker compose up -d

export LOKI_ENDPOINT="${LOKI_ENDPOINT:-http://localhost:3100}"

echo "Loki endpoint: ${LOKI_ENDPOINT}"
echo "Grafana URL: http://localhost:3000 (admin/admin)"
echo "Dashboard UID: drasi-loki-overview"
echo
cargo run
