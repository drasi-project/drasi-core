#!/usr/bin/env bash
set -euo pipefail

RPC_URL="${SUI_RPC_URL:-https://fullnode.mainnet.sui.io:443}"
DEEPBOOK_PACKAGE_ID="${DEEPBOOK_PACKAGE_ID:-0x337f4f4f6567fcd778d5454f27c16c70e2f274cc6377ea6249ddf491482ef497}"
MAX_PAGES="${MAX_PAGES:-100}"

echo "=== DeepBook change detection smoke test ==="
echo "Querying latest DeepBook events from package ${DEEPBOOK_PACKAGE_ID}"
echo

python3 - "${RPC_URL}" "${DEEPBOOK_PACKAGE_ID}" "${MAX_PAGES}" <<'PY'
import json
import sys
import urllib.error
import urllib.request

rpc_url = sys.argv[1]
package_id = sys.argv[2].lower()
max_pages = max(int(sys.argv[3]), 1)

cursor = None
matches = []
pages_scanned = 0

for page in range(max_pages):
    params = [{"All": []}, cursor, 50, True]

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
        print(f"RPC request failed: {err}", file=sys.stderr)
        sys.exit(1)
    except json.JSONDecodeError as err:
        print(f"Failed to parse RPC response as JSON: {err}", file=sys.stderr)
        sys.exit(1)

    if "error" in body:
        print(f"RPC error: {body['error']}", file=sys.stderr)
        sys.exit(1)

    result = body.get("result", {})
    rows = result.get("data", [])
    pages_scanned += 1

    for event in rows:
        if str(event.get("packageId", "")).lower() != package_id:
            continue
        matches.append(event)
        if len(matches) >= 10:
            break

    if len(matches) >= 10:
        break

    cursor = result.get("nextCursor")
    if not result.get("hasNextPage") or cursor is None:
        break

if matches:
    for event in matches:
        event_type = event.get("type", "<unknown>")
        timestamp = event.get("timestampMs", "<unknown>")
        print(f"{event_type} @ {timestamp}")
else:
    print(
        f"No matching DeepBook events found after scanning {pages_scanned} page(s); retry in a few minutes."
    )
PY

echo
echo "If event types are listed above, the source should be able to ingest updates."
