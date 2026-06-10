#!/bin/bash

echo "=== HERE Traffic Source Diagnostics ==="

if [ -z "$HERE_API_KEY" ]; then
    echo "❌ HERE_API_KEY not set"
else
    echo "✓ HERE_API_KEY set (${#HERE_API_KEY} chars)"
fi

BBOX=${HERE_BBOX:-"52.5,13.3,52.6,13.5"}

echo ""
echo "Testing Flow endpoint..."
curl -s "https://data.traffic.hereapi.com/v7/flow?in=bbox:${BBOX}&apiKey=${HERE_API_KEY}" \
    | jq -r 'if .error then "❌ Error: " + .error_description else "✓ Flow endpoint OK" end'

echo ""
echo "Testing Incidents endpoint..."
curl -s "https://data.traffic.hereapi.com/v7/incidents?in=bbox:${BBOX}&apiKey=${HERE_API_KEY}" \
    | jq -r 'if .error then "❌ Error: " + .error_description else "✓ Incidents endpoint OK" end'
