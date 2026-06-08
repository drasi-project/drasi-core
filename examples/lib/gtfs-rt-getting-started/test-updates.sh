#!/usr/bin/env bash
set -euo pipefail

port="${GTFS_RT_DASHBOARD_PORT:-8090}"

while true; do
  echo "--- $(date -u +%H:%M:%S) ---"
  curl -sS "http://localhost:${port}/api/dashboard/state" | python -m json.tool | head -n 80
  echo
  sleep 5
done
