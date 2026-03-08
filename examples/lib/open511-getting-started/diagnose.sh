#!/usr/bin/env bash
set -euo pipefail

API_URL="${OPEN511_API_URL:-https://api.open511.gov.bc.ca}"

echo "=== Open511 Example Diagnostics ==="
echo "API URL: ${API_URL}"

TMPFILE="$(mktemp)"
trap 'rm -f "${TMPFILE}"' EXIT

STATUS="$(curl -s -o "${TMPFILE}" -w "%{http_code}" "${API_URL}/events?status=ACTIVE&limit=5&format=json" || true)"
echo "HTTP status: ${STATUS}"

if [[ "${STATUS}" == "200" ]]; then
  python3 - "${TMPFILE}" <<'PY'
import json, sys
with open(sys.argv[1], "r", encoding="utf-8") as f:
    data = json.load(f)
events = data.get("events", [])
latest = max((e.get("updated","") for e in events), default="")
print(f"Events returned: {len(events)}")
print(f"Latest updated timestamp (sample): {latest}")
PY
  echo "Diagnosis: API reachable and returning JSON."
else
  echo "Diagnosis: API request failed."
  cat "${TMPFILE}" || true
  exit 1
fi
