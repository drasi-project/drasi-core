#!/bin/bash
# Diagnose environment and API access for the Cloudflare Radar dashboard example

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

API_BASE_URL="${CF_RADAR_API_BASE_URL:-https://api.cloudflare.com/client/v4}"

echo "Cloudflare Radar Dashboard Diagnostics"
echo "--------------------------------------"
if [[ -z "${CF_RADAR_TOKEN:-}" ]]; then
    echo "CF_RADAR_TOKEN: NOT SET"
else
    echo "CF_RADAR_TOKEN: SET"
fi
echo "API base URL: ${API_BASE_URL}"

if [[ -n "${CF_RADAR_TOKEN:-}" ]]; then
    status=$(curl -s -o /dev/null -w "%{http_code}" \
        -H "Authorization: Bearer ${CF_RADAR_TOKEN}" \
        "${API_BASE_URL}/radar/annotations/outages?limit=1&format=json")
    echo "Radar API status: ${status}"
else
    echo "Skipping API check because CF_RADAR_TOKEN is not set."
fi

if lsof -ti:8080 > /dev/null 2>&1; then
    echo "Results API detected on port 8080."
    for query_id in active-outages hijacks top-domains; do
        echo ""
        echo "Query results for ${query_id}:"
        curl -s "http://localhost:8080/queries/${query_id}/results" | head -c 500
        echo ""
    done
else
    echo "Results API not detected on port 8080."
    echo "Run ./quickstart.sh to start the dashboard."
fi
