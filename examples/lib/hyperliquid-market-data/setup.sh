#!/bin/bash
set -e

cd "$(dirname "$0")"

echo "Checking Hyperliquid REST API..."
for i in {1..60}; do
  if curl -s -X POST https://api.hyperliquid.xyz/info \
    -H "Content-Type: application/json" \
    -d '{"type":"allMids"}' > /dev/null; then
    echo "✓ REST API reachable"
    break
  fi
  if [ $i -eq 60 ]; then
    echo "✗ Failed to reach Hyperliquid REST API"
    exit 1
  fi
  if [ $((i % 10)) -eq 0 ]; then
    echo "  Still waiting... ($i/60)"
  fi
  sleep 1
done

if command -v wscat >/dev/null 2>&1; then
  echo "Checking Hyperliquid WebSocket..."
  timeout 5 wscat -c wss://api.hyperliquid.xyz/ws \
    -x '{"method":"subscribe","subscription":{"type":"allMids"}}' \
    2>/dev/null | head -2 >/dev/null && echo "✓ WebSocket reachable" || echo "⚠️ WebSocket check failed"
else
  echo "⚠️ wscat not installed (optional). Install with: npm i -g wscat"
fi

echo "✓ Setup complete!"
