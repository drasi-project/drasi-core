#!/usr/bin/env bash
# Copyright 2026 The Drasi Authors.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
export COMPOSE_PROJECT_NAME="inventory-check-$(date +%s)-$$"
export TEMPORAL_UI_PORT=0
compose=(docker compose --file inventory/compose.yaml)
evidence="$PWD/.runs/$COMPOSE_PROJECT_NAME"
mkdir -p "$evidence"
exec > >(tee "$evidence/verify.log") 2>&1

cleanup() {
    result=$?
    trap - EXIT
    "${compose[@]}" logs --no-color > "$evidence/containers.log" 2>&1 || result=1
    "${compose[@]}" down --volumes || result=1
    echo "Evidence: $evidence"
    exit "$result"
}
trap cleanup EXIT

assert_state() {
    "${compose[@]}" exec -T worker python - "$1" "$2" <<'PY'
import asyncio, os, sys
import psycopg
from temporalio.client import Client
from demo import drasi_status, events

async def check():
    client = await Client.connect(os.environ["TEMPORAL_ADDRESS"])
    workflow = client.get_workflow_handle("inventory-monitor")
    received = await events(workflow)
    names = [event["signal"] for event in received]
    expected = sys.argv[2].split(",") if sys.argv[2] else []
    assert names == expected, (names, expected)
    assert (await workflow.describe()).status.name == "RUNNING"
    with psycopg.connect() as db:
        assert db.execute("SELECT stock FROM inventory WHERE product = 'coffee-beans'").fetchone()[0] == int(sys.argv[1])
    status = drasi_status()
    expected_rows = [] if int(sys.argv[1]) >= 5 else [{"product": "coffee-beans", "stock": int(sys.argv[1])}]
    assert status["results"] == expected_rows, status
    assert [event["diff"]["sequenceId"] for event in received] == list(range(1, len(received) + 1))
    for event in received:
        assert event["diff"]["queryId"] == "changes"
        assert event["diff"]["operation"] == {"low_stock": "ADD", "stock_changed": "UPDATE", "stock_restored": "DELETE"}[event["signal"]]
    print(f"PASS: PostgreSQL stock={sys.argv[1]}, Temporal signals={names}, workflow still running")

asyncio.run(check())
PY
}

./demo.sh up
"${compose[@]}" exec -T worker python -c 'from urllib.request import urlopen; response = urlopen("http://temporal:8233", timeout=5); assert response.status == 200; assert b"<html" in response.read(); print("PASS: Temporal Web UI is served")'
assert_state 20 ""
./demo.sh stock 4
assert_state 4 "low_stock"
./demo.sh stock 2
assert_state 2 "low_stock,stock_changed"
./demo.sh stock 20
assert_state 20 "low_stock,stock_changed,stock_restored"
./demo.sh stock 20
./demo.sh stock 8
./demo.sh stock 4
assert_state 4 "low_stock,stock_changed,stock_restored,low_stock"
if ./demo.sh stock -1; then
    echo "Negative stock was accepted" >&2
    exit 1
fi
./demo.sh up
assert_state 4 "low_stock,stock_changed,stock_restored,low_stock"
./demo.sh down
./demo.sh up
assert_state 4 "low_stock,stock_changed,stock_restored,low_stock"
./demo.sh stock 20
assert_state 20 "low_stock,stock_changed,stock_restored,low_stock,stock_restored"
"${compose[@]}" stop drasi
drasi_container="$("${compose[@]}" ps --all --quiet drasi)"
[[ "$(docker inspect --format '{{.State.ExitCode}}' "$drasi_container")" == 0 ]]
if failure="$(./demo.sh stock 3 2>&1)"; then
    echo "A command reported success while Drasi was unavailable" >&2
    exit 1
fi
echo "$failure"
[[ "$failure" == *"Cannot reach Drasi"* ]]
./demo.sh up
assert_state 20 "low_stock,stock_changed,stock_restored,low_stock,stock_restored"
echo "PASS: Docker-only walkthrough, signal payloads, unchanged stock, invalid input, repeated up, restart, and explicit outage failure"
