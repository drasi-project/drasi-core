#!/bin/bash
set -e

cd "$(dirname "$0")"

echo "Testing Hyperliquid REST API..."
if curl -s -X POST https://api.hyperliquid.xyz/info \
  -H "Content-Type: application/json" \
  -d '{"type":"allMids"}' | python3 -c "import sys,json; d=json.load(sys.stdin); print(f'✅ Connected: {len(d)} coins, BTC=${d.get(\"BTC\",\"N/A\")}')" 2>/dev/null; then
  true
else
  echo "❌ REST API unreachable"
fi

echo "Testing Hyperliquid WebSocket..."
if command -v wscat >/dev/null 2>&1; then
  timeout 5 wscat -c wss://api.hyperliquid.xyz/ws \
    -x '{"method":"subscribe","subscription":{"type":"allMids"}}' \
    2>/dev/null | head -2 >/dev/null && echo "✅ WebSocket connected" || echo "⚠️ WebSocket unreachable"
else
  echo "⚠️ wscat not installed (optional). Install with: npm i -g wscat"
fi
