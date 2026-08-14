#!/usr/bin/env bash
set -euo pipefail

GRAPHQL_ADDR="${GITHUB_EXAMPLE_GRAPHQL_ADDR:-127.0.0.1:19080}"
WEBHOOK_HOST="${GITHUB_EXAMPLE_WEBHOOK_HOST:-127.0.0.1}"
WEBHOOK_PORT="${GITHUB_EXAMPLE_WEBHOOK_PORT:-19081}"
WEBHOOK_PATH="${GITHUB_EXAMPLE_WEBHOOK_PATH:-/webhook}"
WEBHOOK_SECRET="${GITHUB_EXAMPLE_WEBHOOK_SECRET:-example-secret}"

sign_payload() {
  local body="$1"
  python3 - "${WEBHOOK_SECRET}" "${body}" <<'PY'
import hashlib, hmac, sys
secret = sys.argv[1].encode("utf-8")
body = sys.argv[2].encode("utf-8")
print("sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest())
PY
}

post_control() {
  local payload="$1"
  curl -fsS -X POST \
    -H "Content-Type: application/json" \
    -d "${payload}" \
    "http://${GRAPHQL_ADDR}/control/issue" >/dev/null
}

send_webhook() {
  local delivery="$1"
  local event="$2"
  local payload="$3"
  local signature
  signature="$(sign_payload "${payload}")"

  curl -sS -o /dev/null -w "%{http_code}" \
    -X POST \
    -H "X-Hub-Signature-256: ${signature}" \
    -H "X-GitHub-Delivery: ${delivery}" \
    -H "X-GitHub-Event: ${event}" \
    -H "Content-Type: application/json" \
    -d "${payload}" \
    "http://${WEBHOOK_HOST}:${WEBHOOK_PORT}${WEBHOOK_PATH}"
}

echo "Running CREATE/UPDATE/DELETE webhook sequence..."
echo "Make sure the example is running in another terminal (./quickstart.sh)."

delivery_prefix="example-$(date +%s)"

echo "1) CREATE"
post_control '{"exists":true,"title":"Issue created from webhook"}'
status="$(send_webhook "${delivery_prefix}-create" "issues" '{"action":"opened","issue":{"node_id":"I_1"},"repository":{"full_name":"acme/repo"}}')"
echo "   webhook status=${status}"
[[ "${status}" == "200" ]] || { echo "✗ CREATE webhook failed"; exit 1; }
sleep 1

echo "2) UPDATE"
post_control '{"exists":true,"title":"Issue updated from webhook"}'
status="$(send_webhook "${delivery_prefix}-update" "issues" '{"action":"edited","issue":{"node_id":"I_1"},"repository":{"full_name":"acme/repo"}}')"
echo "   webhook status=${status}"
[[ "${status}" == "200" ]] || { echo "✗ UPDATE webhook failed"; exit 1; }
sleep 1

echo "3) DELETE"
post_control '{"exists":false}'
status="$(send_webhook "${delivery_prefix}-delete" "issues" '{"action":"deleted","issue":{"node_id":"I_1"},"repository":{"full_name":"acme/repo"}}')"
echo "   webhook status=${status}"
[[ "${status}" == "200" ]] || { echo "✗ DELETE webhook failed"; exit 1; }
sleep 1

echo "✓ Sent CREATE/UPDATE/DELETE webhooks."
echo "Check the running example logs for:"
echo "  ➕ ISSUE INSERTED"
echo "  🔄 ISSUE UPDATED"
echo "  ➖ ISSUE DELETED"
