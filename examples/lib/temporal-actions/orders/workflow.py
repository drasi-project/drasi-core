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
class OrderLifecycle:
    def __init__(self):
        self.status = "awaiting_placement"
        self.events = []

    @workflow.run
    async def run(self):
        for stage in ("placed", "paid", "shipped", "delivered"):
            await workflow.wait_condition(
                lambda: any(event["signal"] == stage for event in self.events)
            )
            self.status = stage
        return self.progress()

    @workflow.signal
    def placed(self, diff: dict):
        self.events.append({"signal": "placed", "diff": diff})

    @workflow.signal
    def paid(self, diff: dict):
        self.events.append({"signal": "paid", "diff": diff})

    @workflow.signal
    def shipped(self, diff: dict):
        self.events.append({"signal": "shipped", "diff": diff})

    @workflow.signal
    def delivered(self, diff: dict):
        self.events.append({"signal": "delivered", "diff": diff})

    @workflow.query
    def progress(self) -> dict:
        return {"status": self.status, "events": self.events}


async def main():
    client = await Client.connect(os.environ["TEMPORAL_ADDRESS"])
    async with Worker(client, task_queue="orders", workflows=[OrderLifecycle]):
        try:
            await client.start_workflow(
                OrderLifecycle.run, id="order-1001", task_queue="orders",
                id_reuse_policy=WorkflowIDReusePolicy.REJECT_DUPLICATE,
            )
        except WorkflowAlreadyStartedError:
            current = await client.get_workflow_handle("order-1001").describe()
            if current.status not in (WorkflowExecutionStatus.RUNNING, WorkflowExecutionStatus.COMPLETED):
                raise RuntimeError("order-1001 did not complete normally; use a fresh demo project")
        print("Order Worker ready", flush=True)
        await asyncio.Event().wait()


if __name__ == "__main__":
    asyncio.run(main())
