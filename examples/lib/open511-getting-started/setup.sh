#!/usr/bin/env bash
set -euo pipefail

API_URL="${OPEN511_API_URL:-https://api.open511.gov.bc.ca}"
TIMEOUT=60

echo "Initializing Open511 example against: ${API_URL}"
echo "Checking API health (timeout: ${TIMEOUT}s)..."

for i in $(seq 1 "${TIMEOUT}"); do
  if curl -fsS "${API_URL}/events?status=ACTIVE&limit=1&format=json" >/dev/null 2>&1; then
    echo "Open511 API is reachable."
    exit 0
  fi

  if (( i % 10 == 0 )); then
    echo "  still waiting... (${i}/${TIMEOUT})"
  fi
  sleep 1
done

echo "Failed to reach Open511 API after ${TIMEOUT}s"
echo "Diagnostics:"
curl -v "${API_URL}/events?status=ACTIVE&limit=1&format=json" --max-time 10 || true
exit 1
