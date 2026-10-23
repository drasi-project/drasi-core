# Copyright 2026 The Drasi Authors.
# SPDX-License-Identifier: Apache-2.0

"""The example owns this workflow; the Drasi reaction only sends signals."""

import asyncio
from temporalio import workflow

with workflow.unsafe.imports_passed_through():
    import os
    from temporalio.client import Client, WorkflowExecutionStatus
    from temporalio.common import WorkflowIDReusePolicy
    from temporalio.exceptions import WorkflowAlreadyStartedError
    from temporalio.worker import Worker


@workflow.defn
class InventoryMonitor:
    def __init__(self):
        self.events = []

    @workflow.run
    async def run(self):
        await workflow.wait_condition(lambda: False)

    @workflow.signal
    def low_stock(self, diff: dict):
        self.events.append({"signal": "low_stock", "diff": diff})

    @workflow.signal
    def stock_changed(self, diff: dict):
        self.events.append({"signal": "stock_changed", "diff": diff})

    @workflow.signal
    def stock_restored(self, diff: dict):
        self.events.append({"signal": "stock_restored", "diff": diff})

    @workflow.query
    def events_received(self) -> list:
        return self.events


async def main():
    client = await Client.connect(os.environ["TEMPORAL_ADDRESS"])
    async with Worker(client, task_queue="inventory", workflows=[InventoryMonitor]):
        try:
            await client.start_workflow(
                InventoryMonitor.run, id="inventory-monitor", task_queue="inventory",
                id_reuse_policy=WorkflowIDReusePolicy.REJECT_DUPLICATE,
            )
        except WorkflowAlreadyStartedError:
            current = await client.get_workflow_handle("inventory-monitor").describe()
            if current.status != WorkflowExecutionStatus.RUNNING:
                raise RuntimeError("inventory-monitor is closed; use a fresh demo project")
        print("Inventory monitor ready", flush=True)
        await asyncio.Event().wait()


if __name__ == "__main__":
    asyncio.run(main())
