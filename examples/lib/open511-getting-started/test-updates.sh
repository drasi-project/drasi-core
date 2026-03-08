#!/usr/bin/env bash
set -euo pipefail

API_URL="${OPEN511_API_URL:-https://api.open511.gov.bc.ca}"

echo "Monitoring Open511 updates from: ${API_URL}"
echo "Polling every 60 seconds. Press Ctrl+C to stop."

prev_count=""
prev_latest=""

while true; do
  body="$(curl -fsS "${API_URL}/events?status=ACTIVE&limit=500&format=json")"
  count="$(python3 -c 'import json,sys; d=json.load(sys.stdin); print(len(d.get("events", [])))' <<<"${body}")"
  latest="$(python3 -c 'import json,sys; d=json.load(sys.stdin); print(max((e.get("updated","") for e in d.get("events", [])), default=""))' <<<"${body}")"

  echo "$(date -Iseconds) active_count=${count} latest_updated=${latest}"

  if [[ -n "${prev_count}" && "${count}" != "${prev_count}" ]]; then
    echo "  change detected: active_count ${prev_count} -> ${count}"
  fi
  if [[ -n "${prev_latest}" && "${latest}" != "${prev_latest}" ]]; then
    echo "  change detected: latest_updated ${prev_latest} -> ${latest}"
  fi

  prev_count="${count}"
  prev_latest="${latest}"
  sleep 60
done
