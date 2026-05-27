#!/usr/bin/env bash
set -euo pipefail

RPC_URL="${SUI_RPC_URL:-https://fullnode.mainnet.sui.io:443}"

echo "=== Sui DeepBook setup ==="
echo "RPC endpoint: ${RPC_URL}"
echo "Checking endpoint health (60s timeout)..."

if ! command -v curl >/dev/null 2>&1; then
  echo "✗ curl is required"
  exit 1
fi

for i in {1..30}; do
  response="$(curl -s -X POST "${RPC_URL}" \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":1,"method":"sui_getChainIdentifier","params":[]}' || true)"

  if echo "${response}" | grep -q '"result"'; then
    chain_id="$(echo "${response}" | sed -n 's/.*"result":"\([^"]*\)".*/\1/p')"
    echo "✓ Sui RPC reachable (chain id: ${chain_id:-unknown})"
    echo "✓ Setup complete"
    exit 0
  fi

  if [ "${i}" -eq 30 ]; then
    echo "✗ RPC endpoint did not become healthy within 60 seconds"
    echo "Last response:"
    echo "${response}"
    exit 1
  fi

  if [ $((i % 5)) -eq 0 ]; then
    echo "  still waiting... (${i}/30)"
  fi
  sleep 2
done
