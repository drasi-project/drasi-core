#!/usr/bin/env bash
set -euo pipefail

RPC_URL="${SUI_RPC_URL:-https://fullnode.mainnet.sui.io:443}"
DEEPBOOK_PACKAGE_ID="${DEEPBOOK_PACKAGE_ID:-0x337f4f4f6567fcd778d5454f27c16c70e2f274cc6377ea6249ddf491482ef497}"
MAX_PAGES="${MAX_PAGES:-100}"

echo "=== Sui DeepBook diagnostics ==="
echo "RPC endpoint: ${RPC_URL}"
echo "DeepBook package: ${DEEPBOOK_PACKAGE_ID}"
echo

echo "1) Chain identifier:"
curl -s -X POST "${RPC_URL}" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"sui_getChainIdentifier","params":[]}'
echo
echo

echo "2) Recent DeepBook package events:"
python3 - "${RPC_URL}" "${DEEPBOOK_PACKAGE_ID}" "${MAX_PAGES}" <<'PY'
import json
import sys
import urllib.error
import urllib.request

rpc_url = sys.argv[1]
package_id = sys.argv[2].lower()
max_pages = max(int(sys.argv[3]), 1)

cursor = None
pages_scanned = 0
total_rows = 0
matches = []

for page in range(max_pages):
    params = {
        "query": {"All": []},
        "limit": 50,
        "descending_order": True,
    }
    if cursor is not None:
        params["cursor"] = cursor

    payload = {
        "jsonrpc": "2.0",
        "id": page + 1,
        "method": "suix_queryEvents",
        "params": params,
    }

    request = urllib.request.Request(
        rpc_url,
        data=json.dumps(payload).encode("utf-8"),
        headers={"Content-Type": "application/json"},
    )

    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            body = json.load(response)
    except urllib.error.URLError as err:
        print(f"RPC request failed: {err}")
        sys.exit(1)
    except json.JSONDecodeError as err:
        print(f"Failed to parse RPC response as JSON: {err}")
        sys.exit(1)

    if "error" in body:
        print(f"RPC error: {body['error']}")
        sys.exit(1)

    result = body.get("result", {})
    rows = result.get("data", [])
    pages_scanned += 1
    total_rows += len(rows)

    for event in rows:
        if str(event.get("packageId", "")).lower() == package_id:
            matches.append(event)
            if len(matches) >= 5:
                break

    if len(matches) >= 5:
        break

    cursor = result.get("nextCursor")
    if not result.get("hasNextPage") or cursor is None:
        break

print(f"Scanned {pages_scanned} page(s), {total_rows} event(s); found {len(matches)} match(es).")
for event in matches:
    event_type = event.get("type", "<unknown>")
    timestamp = event.get("timestampMs", "<unknown>")
    event_id = event.get("id", {})
    tx = event_id.get("txDigest", "<unknown>")
    seq = event_id.get("eventSeq", "<unknown>")
    print(f"- {event_type} @ {timestamp} (tx={tx}, seq={seq})")
PY

echo

echo "✓ Diagnostics complete"
