#!/bin/bash
set -e

cd "$(dirname "$0")"

echo "Subscribing to BTC trades for 10 seconds..."
if command -v wscat >/dev/null 2>&1; then
  count=$(timeout 10 wscat -c wss://api.hyperliquid.xyz/ws \
    -x '{"method":"subscribe","subscription":{"type":"trades","coin":"BTC"}}' \
    2>/dev/null | grep -c '"trades"' || true)
  echo "Received ${count} trade messages in 10 seconds"
else
  echo "⚠️ wscat not installed. Install with: npm i -g wscat"
fi
