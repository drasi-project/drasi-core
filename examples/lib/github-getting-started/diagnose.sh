#!/usr/bin/env bash
set -euo pipefail

GRAPHQL_ADDR="${GITHUB_EXAMPLE_GRAPHQL_ADDR:-127.0.0.1:19080}"
WEBHOOK_HOST="${GITHUB_EXAMPLE_WEBHOOK_HOST:-127.0.0.1}"
WEBHOOK_PORT="${GITHUB_EXAMPLE_WEBHOOK_PORT:-19081}"

echo "=== GitHub Source Example Diagnostics ==="
echo "Mock GraphQL : http://${GRAPHQL_ADDR}/healthz"
echo "Webhook      : http://${WEBHOOK_HOST}:${WEBHOOK_PORT}/health"
echo

status_graphql="$(curl -s -o /dev/null -w "%{http_code}" "http://${GRAPHQL_ADDR}/healthz" || true)"
status_webhook="$(curl -s -o /dev/null -w "%{http_code}" "http://${WEBHOOK_HOST}:${WEBHOOK_PORT}/health" || true)"

echo "mock_graphql_health_status=${status_graphql}"
echo "webhook_health_status=${status_webhook}"

if [[ "${status_graphql}" != "200" || "${status_webhook}" != "200" ]]; then
  echo "✗ One or more endpoints are unhealthy/unreachable."
  echo "  Start the example first: ./quickstart.sh"
  exit 1
fi

echo
echo "Current control state:"
curl -fsS "http://${GRAPHQL_ADDR}/control/issue" | python3 -m json.tool

echo
echo "Webhook health payload:"
curl -fsS "http://${WEBHOOK_HOST}:${WEBHOOK_PORT}/health" | python3 -m json.tool

echo
echo "✓ Diagnostics complete"
