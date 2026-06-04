#!/bin/bash
# Poll the results API to observe changes over time

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if ! lsof -ti:8080 > /dev/null 2>&1; then
    echo "Results API not detected on port 8080."
    echo "Run ./quickstart.sh to start the dashboard."
    exit 1
fi

INTERVAL_SECS="${CF_RADAR_TEST_INTERVAL_SECS:-60}"

echo "Polling results API every ${INTERVAL_SECS}s. Press Ctrl+C to stop."
while true; do
    echo ""
    echo "=== $(date -u +"%Y-%m-%dT%H:%M:%SZ") ==="
    for query_id in active-outages hijacks top-domains; do
        echo ""
        echo "Query results for ${query_id}:"
        curl -s "http://localhost:8080/queries/${query_id}/results" | head -c 500
        echo ""
    done
    sleep "${INTERVAL_SECS}"
done
