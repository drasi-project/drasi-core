#!/usr/bin/env bash
set -euo pipefail

GRAPHQL_ADDR="${GITHUB_EXAMPLE_GRAPHQL_ADDR:-127.0.0.1:19080}"
WEBHOOK_HOST="${GITHUB_EXAMPLE_WEBHOOK_HOST:-127.0.0.1}"
WEBHOOK_PORT="${GITHUB_EXAMPLE_WEBHOOK_PORT:-19081}"
DATA_DIR="${GITHUB_EXAMPLE_DATA_DIR:-.data}"
TIMEOUT=60

GRAPHQL_HOST="${GRAPHQL_ADDR%:*}"
GRAPHQL_PORT="${GRAPHQL_ADDR##*:}"

require_cmd() {
  local cmd="$1"
  if ! command -v "${cmd}" >/dev/null 2>&1; then
    echo "✗ Missing required command: ${cmd}"
    exit 1
  fi
}

check_port_free() {
  local host="$1"
  local port="$2"
  python3 - "$host" "$port" <<'PY'
import socket, sys
host = sys.argv[1]
port = int(sys.argv[2])
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
try:
    s.bind((host, port))
except OSError:
    sys.exit(1)
finally:
    s.close()
PY
}

wait_for_port_free() {
  local host="$1"
  local port="$2"
  local label="$3"
  echo "Checking ${label} port ${host}:${port} (timeout ${TIMEOUT}s)..."
  for i in $(seq 1 "${TIMEOUT}"); do
    if check_port_free "${host}" "${port}"; then
      echo "✓ ${label} port is available"
      return 0
    fi

    if (( i % 10 == 0 )); then
      echo "  still waiting for ${label} port to become free... (${i}/${TIMEOUT})"
    fi
    sleep 1
  done

  echo "✗ ${label} port ${host}:${port} is still in use after ${TIMEOUT}s"
  if command -v lsof >/dev/null 2>&1; then
    echo "Port diagnostics:"
    lsof -nP -iTCP:"${port}" -sTCP:LISTEN || true
  fi
  return 1
}

echo "Preparing GitHub source example environment..."
require_cmd cargo
require_cmd curl
require_cmd python3

mkdir -p "${DATA_DIR}"
touch "${DATA_DIR}/.write_test" && rm -f "${DATA_DIR}/.write_test"
echo "✓ Data directory writable: ${DATA_DIR}"

wait_for_port_free "${GRAPHQL_HOST}" "${GRAPHQL_PORT}" "mock-graphql"
wait_for_port_free "${WEBHOOK_HOST}" "${WEBHOOK_PORT}" "webhook-listener"

echo "✓ Setup complete"
