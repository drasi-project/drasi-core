# Copyright 2026 The Drasi Authors.
# SPDX-License-Identifier: Apache-2.0

"""Drive real SQL changes and read the resulting Temporal workflow state."""

import asyncio
import json
import os
import sys
import time
from datetime import timedelta
from urllib.error import HTTPError, URLError
from urllib.request import urlopen

import psycopg
from temporalio.client import Client, WorkflowExecutionStatus
from temporalio.service import RPCError

ORDER_ID = "1001"
STAGES = ("placed", "paid", "shipped", "delivered")
RPC_TIMEOUT = timedelta(seconds=5)


def drasi_status():
    try:
        with urlopen("http://drasi:8080/status", timeout=3) as response:
            return json.load(response)
    except HTTPError as error:
        raise RuntimeError(f"Drasi is not healthy: {error.read().decode()}") from error
    except URLError as error:
        raise RuntimeError(
            f"Cannot reach Drasi; run ./demo.sh up and check its container logs. ({error.reason})"
        ) from error


async def progress(handle):
    execution = await handle.describe(rpc_timeout=RPC_TIMEOUT)
    if execution.status == WorkflowExecutionStatus.COMPLETED:
        state = await handle.result(rpc_timeout=RPC_TIMEOUT)
    elif execution.status == WorkflowExecutionStatus.RUNNING:
        state = await handle.query("progress", rpc_timeout=RPC_TIMEOUT)
    else:
        raise RuntimeError(f"Order workflow is {execution.status.name}; use a fresh demo project.")
    return {**state, "workflowStatus": execution.status.name}


async def wait_for_stage(handle, stage):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        status = drasi_status()
        state = await progress(handle)
        for event in state["events"]:
            if event["signal"] != stage:
                continue
            diff = event["diff"]
            if (diff["queryId"] != stage or diff["operation"] != "ADD"
                    or diff["after"] != {"orderId": ORDER_ID, "status": stage}):
                raise RuntimeError(f"Workflow payload does not match the order change: {diff}")
            checkpoint = status["queries"][stage]["checkpoint"]
            if (checkpoint is not None and checkpoint["sequence"] >= diff["sequenceId"]
                    and state["status"] in STAGES
                    and STAGES.index(state["status"]) >= STAGES.index(stage)
                    and (stage != "delivered" or state["workflowStatus"] == "COMPLETED")):
                return state
        await asyncio.sleep(0.1)
    raise TimeoutError(
        f"The database has committed status {stage}, but its Temporal signal was not confirmed "
        "within 30 seconds. Do not assume delivery failed; inspect the workflow and logs before retrying."
    )


async def main():
    action = sys.argv[1]
    if action not in (*STAGES, "ready", "health", "inspect"):
        raise ValueError("expected placed, paid, shipped, delivered, ready, health, or inspect")
    client = await Client.connect(os.environ["TEMPORAL_ADDRESS"])
    handle = client.get_workflow_handle(f"order-{ORDER_ID}")
    if action == "health":
        await progress(handle)
        return
    if action == "inspect":
        print(json.dumps({"workflow": await progress(handle), "drasi": drasi_status()}))
        return
    with psycopg.connect(connect_timeout=5, autocommit=True) as db:
        if not db.execute("SELECT pg_try_advisory_lock(72335432)").fetchone()[0]:
            raise RuntimeError("Another demo command is running; wait for it to finish.")
        drasi_status()
        state = await progress(handle)
        row = db.execute("SELECT status FROM orders WHERE id = %s", (ORDER_ID,)).fetchone()
        before = row[0] if row else None
        if before is not None:
            state = await wait_for_stage(handle, before)
        elif state["status"] != "awaiting_placement":
            raise RuntimeError("The database and workflow do not match; use a fresh demo project.")
        next_stage = STAGES[0] if before is None else (
            STAGES[STAGES.index(before) + 1] if before != "delivered" else None
        )
        if action == "ready":
            print(f"Ready. Order {ORDER_ID}: {before or 'not placed yet'}.")
            print(f"Temporal: order-{ORDER_ID} is {state['workflowStatus']}.")
            if next_stage:
                print(f"Try: ./demo.sh {next_stage}")
            else:
                print("This order is delivered. Use a fresh Compose project to run another order.")
            return
        if before == action:
            print(f"Order {ORDER_ID} is already {action}; its signal is confirmed. Nothing changed.")
            return
        if action != next_stage:
            expected = f"Run ./demo.sh {next_stage} next." if next_stage else "The order is already delivered."
            raise ValueError(f"Cannot move order {ORDER_ID} from {before or 'not placed'} to {action}. {expected}")
        if before is None:
            db.execute("INSERT INTO orders (id, status) VALUES (%s, %s)", (ORDER_ID, action))
            print(f"PostgreSQL: order {ORDER_ID} placed", flush=True)
        else:
            db.execute("UPDATE orders SET status = %s WHERE id = %s", (action, ORDER_ID))
            print(f"PostgreSQL: order {ORDER_ID} changed from {before} to {action}", flush=True)
        await wait_for_stage(handle, action)
        print(f"Drasi:     order entered the {action} query")
        print(f"Temporal:  order-{ORDER_ID} processed {action}")
        if action == "delivered":
            print("Temporal:  workflow completed")


if __name__ == "__main__":
    try:
        asyncio.run(asyncio.wait_for(main(), timeout=45))
    except (RuntimeError, ValueError, TimeoutError, OSError, psycopg.Error, RPCError) as error:
        message = str(error) or "Timed out waiting for the demo services."
        print(f"Demo failed: {message}", file=sys.stderr)
        sys.exit(1)
