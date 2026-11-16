# Copyright 2026 The Drasi Authors.
# SPDX-License-Identifier: Apache-2.0

"""The example owns this workflow; Drasi starts it and sends subsequent signals."""

import asyncio
from temporalio import workflow

with workflow.unsafe.imports_passed_through():
    import os
    from temporalio.client import Client
    from temporalio.worker import Worker


@workflow.defn
class OrderLifecycle:
    def __init__(self):
        self.status = "awaiting_placement"
        self.events = []

    @workflow.run
    async def run(self, placed: dict):
        self.events.insert(0, {"stage": "placed", "diff": placed})
        self.status = "placed"
        for stage in ("paid", "shipped", "delivered"):
            await workflow.wait_condition(
                lambda: any(event["stage"] == stage for event in self.events)
            )
            self.status = stage
        return self.progress()

    @workflow.signal
    def paid(self, diff: dict):
        self.events.append({"stage": "paid", "diff": diff})

    @workflow.signal
    def shipped(self, diff: dict):
        self.events.append({"stage": "shipped", "diff": diff})

    @workflow.signal
    def delivered(self, diff: dict):
        self.events.append({"stage": "delivered", "diff": diff})

    @workflow.query
    def progress(self) -> dict:
        return {"status": self.status, "events": self.events}


async def main():
    client = await Client.connect(os.environ["TEMPORAL_ADDRESS"])
    worker = Worker(client, task_queue="orders", workflows=[OrderLifecycle])
    print("Order Worker starting", flush=True)
    await worker.run()


if __name__ == "__main__":
    asyncio.run(main())
