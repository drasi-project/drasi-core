#!/usr/bin/env bash
# Copyright 2026 The Drasi Authors.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"
export COMPOSE_PROJECT_NAME="orders-check-$(date +%s)-$$"
export TEMPORAL_UI_PORT=0
compose=(docker compose --file orders/compose.yaml)
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
    "${compose[@]}" exec -T worker python - "$1" "$run_id" <<'PY'
import asyncio, os, sys
import psycopg
from temporalio.client import Client
from demo import ORDER_ID, STAGES, drasi_status, progress

async def check():
    client = await Client.connect(os.environ["TEMPORAL_ADDRESS"])
    workflow = client.get_workflow_handle(f"order-{ORDER_ID}")
    current = sys.argv[1]
    expected = list(STAGES[:STAGES.index(current) + 1]) if current in STAGES else []
    state = await progress(workflow)
    names = [event["signal"] for event in state["events"]]
    assert names == expected, (names, expected)
    assert state["status"] == current, state
    assert state["workflowStatus"] == ("COMPLETED" if current == "delivered" else "RUNNING"), state
    assert (await workflow.describe()).run_id == sys.argv[2], "Startup replaced the workflow execution"
    history_signals = [
        event.workflow_execution_signaled_event_attributes.signal_name
        async for event in workflow.fetch_history_events()
        if event.HasField("workflow_execution_signaled_event_attributes")
    ]
    assert history_signals == expected, history_signals
    with psycopg.connect() as db:
        rows = db.execute("SELECT id, status FROM orders").fetchall()
        assert rows == ([(ORDER_ID, current)] if expected else []), rows
    for event in state["events"]:
        diff = event["diff"]
        assert diff["queryId"] == event["signal"]
        assert diff["operation"] == "ADD"
        assert diff["sequenceId"] == 1
        assert diff["after"] == {"orderId": ORDER_ID, "status": event["signal"]}
    for _ in range(100):
        queries = drasi_status()["queries"]
        settled = True
        for stage in STAGES:
            expected_rows = [{"orderId": ORDER_ID, "status": current}] if stage == current else []
            checkpoint = queries[stage]["checkpoint"]
            sequence = 1 if stage == current else 2 if stage in expected else 0
            settled &= queries[stage]["results"] == expected_rows
            settled &= checkpoint is not None and checkpoint["sequence"] == sequence
        if settled:
            break
        await asyncio.sleep(0.1)
    assert settled, queries
    print(f"PASS: order={current}, signals={names}, execution={state['workflowStatus']}, same run ID")

asyncio.run(check())
PY
}

reject_stage() {
    if failure="$(./demo.sh "$1" 2>&1)"; then
        echo "An invalid transition to $1 was accepted" >&2
        exit 1
    fi
    echo "$failure"
    [[ "$failure" == *"Cannot move order"* ]]
}

./demo.sh up
"${compose[@]}" exec -T worker python -m unittest -v test_demo
run_id="$("${compose[@]}" exec -T worker python -c 'import asyncio, os; from temporalio.client import Client
async def run():
    client = await Client.connect(os.environ["TEMPORAL_ADDRESS"])
    print((await client.get_workflow_handle("order-1001").describe()).run_id)
asyncio.run(run())')"
"${compose[@]}" exec -T worker python -c 'from urllib.request import urlopen; response = urlopen("http://temporal:8233", timeout=5); assert response.status == 200; assert b"<html" in response.read(); print("PASS: Temporal Web UI is served")'
assert_state awaiting_placement
reject_stage paid
assert_state awaiting_placement
./demo.sh placed
assert_state placed
./demo.sh placed
assert_state placed
reject_stage shipped
assert_state placed
./demo.sh paid
assert_state paid
if ./demo.sh invalid; then
    echo "An unknown stage was accepted" >&2
    exit 1
fi
./demo.sh up
assert_state paid
./demo.sh down
./demo.sh up
assert_state paid
"${compose[@]}" stop drasi
drasi_container="$("${compose[@]}" ps --all --quiet drasi)"
[[ "$(docker inspect --format '{{.State.ExitCode}}' "$drasi_container")" == 0 ]]
if failure="$(./demo.sh shipped 2>&1)"; then
    echo "A command reported success while Drasi was unavailable" >&2
    exit 1
fi
echo "$failure"
[[ "$failure" == *"Cannot reach Drasi"* ]]
./demo.sh up
assert_state paid
./demo.sh shipped
assert_state shipped
./demo.sh delivered
assert_state delivered
./demo.sh delivered
assert_state delivered
reject_stage placed
./demo.sh down
./demo.sh up
assert_state delivered
echo "PASS: four SQL-driven signals, workflow completion, payloads, no-op, invalid transitions, repeated up, restart before/after completion, and explicit outage failure"

"${compose[@]}" stop drasi worker
"${compose[@]}" run --rm --no-deps -T worker python - "$run_id" <<'PY'
import asyncio, os, sys
import psycopg
from temporalio.api.common.v1 import WorkflowExecution
from temporalio.api.workflowservice.v1 import DeleteWorkflowExecutionRequest
from temporalio.client import Client
from temporalio.service import RPCError, RPCStatusCode
from workflow import OrderLifecycle, main

async def check():
    client = await Client.connect(os.environ["TEMPORAL_ADDRESS"])
    handle = client.get_workflow_handle("order-1001")
    await client.workflow_service.delete_workflow_execution(DeleteWorkflowExecutionRequest(
        namespace="default",
        workflow_execution=WorkflowExecution(workflow_id="order-1001", run_id=sys.argv[1]),
    ))
    async def missing():
        try:
            await handle.describe()
        except RPCError as error:
            if error.status != RPCStatusCode.NOT_FOUND:
                raise
            return True
        return False
    async with asyncio.timeout(15):
        while not await missing():
            await asyncio.sleep(0.1)
        try:
            await main()
        except RuntimeError as error:
            assert "history is missing" in str(error), error
        else:
            raise AssertionError("Startup accepted an order whose workflow history was deleted")
    assert await missing(), "Startup recreated the expired workflow"
    with psycopg.connect() as db:
        assert db.execute("SELECT id, status FROM orders").fetchall() == [("1001", "delivered")]
    print("PASS: missing workflow history fails startup without a replacement or SQL mutation")
    await client.start_workflow(OrderLifecycle.run, id="order-1001", task_queue="orders")

asyncio.run(check())
PY
"${compose[@]}" up --detach --wait --wait-timeout 120 worker drasi
if failure="$("${compose[@]}" exec -T worker python demo.py ready 2>&1)"; then
    echo "An empty replacement workflow was accepted for the delivered order" >&2
    exit 1
fi
echo "$failure"
[[ "$failure" == *"history is missing previously confirmed stages"* ]]
echo "PASS: an already-replaced workflow is rejected immediately instead of waiting for stale signals"
