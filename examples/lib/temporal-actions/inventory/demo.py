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
from temporalio.client import Client
from temporalio.service import RPCError


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


async def events(handle):
    return await handle.query("events_received", rpc_timeout=timedelta(seconds=5))


async def wait_for_signal(handle, sequence, expected):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        status = drasi_status()
        received = await events(handle)
        for event in received:
            if event["diff"]["sequenceId"] == sequence:
                if event["signal"] != expected:
                    raise RuntimeError(f"Expected {expected}, but the workflow processed {event['signal']}")
                if status["checkpoint"]["sequence"] >= sequence:
                    return event
        await asyncio.sleep(0.1)
    raise TimeoutError(
        "The database commit succeeded, but its Temporal signal was not confirmed within 30 seconds. "
        "Do not assume delivery failed; inspect the workflow and container logs before retrying."
    )


async def main():
    action = sys.argv[1]
    client = await Client.connect(os.environ["TEMPORAL_ADDRESS"])
    handle = client.get_workflow_handle("inventory-monitor")
    if action == "health":
        await events(handle)
        return
    if action == "inspect":
        print(json.dumps({"events": await events(handle), "drasi": drasi_status()}))
        return
    if action not in ("seed", "stock"):
        raise ValueError("expected seed, stock, health, or inspect")
    with psycopg.connect(connect_timeout=5, autocommit=True) as db:
        if not db.execute("SELECT pg_try_advisory_lock(72335432)").fetchone()[0]:
            raise RuntimeError("Another demo command is running; wait for it to finish.")
        received = await events(handle)
        status = drasi_status()
        if action == "seed":
            db.execute("INSERT INTO inventory (product, stock) VALUES (%s, %s) ON CONFLICT DO NOTHING",
                       ("coffee-beans", 20))
            stock = db.execute("SELECT stock FROM inventory WHERE product = %s", ("coffee-beans",)).fetchone()[0]
            print(f"Ready. PostgreSQL has coffee-beans with stock {stock}.")
            print("Drasi watches stock < 5. Temporal's inventory-monitor is running.")
            return
        stock = int(sys.argv[2])
        if not 0 <= stock <= 999_999_999:
            raise ValueError("Stock must be an integer between 0 and 999999999.")
        row = db.execute("SELECT stock FROM inventory WHERE product = %s", ("coffee-beans",)).fetchone()
        if row is None:
            raise RuntimeError("The demo has not been initialized. Run ./demo.sh up first.")
        before = row[0]
        if before == stock:
            print(f"PostgreSQL: stock is already {stock}; nothing changed.")
            return
        # A command must not misattribute an earlier unconfirmed signal to its own update.
        checkpoint = status["checkpoint"]["sequence"]
        if checkpoint and not any(e["diff"]["sequenceId"] == checkpoint for e in received):
            raise RuntimeError("An earlier signal has not finished processing. Retry this command shortly.")
        db.execute("UPDATE inventory SET stock = %s WHERE product = %s", (stock, "coffee-beans"))
        print(f"PostgreSQL: stock changed from {before} to {stock}", flush=True)
        if before >= 5 and stock >= 5:
            print("Drasi:     both values are outside the low-stock query.")
            print("Temporal:  no signal is expected for this change.")
            return
        expected = "low_stock" if before >= 5 else "stock_restored" if stock >= 5 else "stock_changed"
        event = await wait_for_signal(handle, checkpoint + 1, expected)
        diff = event["diff"]
        expected_row = {"product": "coffee-beans", "stock": before if stock >= 5 else stock}
        actual_row = diff["before"] if stock >= 5 else diff["after"]
        if actual_row != expected_row:
            raise RuntimeError(f"Workflow payload does not match the database change: {actual_row}")
        messages = {"ADD": "product entered the low-stock query",
                    "UPDATE": "product changed within the low-stock query",
                    "DELETE": "product left the low-stock query"}
        print(f"Drasi:     {messages[diff['operation']]}")
        print(f"Temporal:  inventory-monitor processed {event['signal']}")


if __name__ == "__main__":
    try:
        asyncio.run(asyncio.wait_for(main(), timeout=45))
    except (RuntimeError, ValueError, TimeoutError, OSError, psycopg.Error, RPCError) as error:
        message = str(error) or "Timed out waiting for the demo services."
        print(f"Demo failed: {message}", file=sys.stderr)
        sys.exit(1)
