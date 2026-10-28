# Copyright 2026 The Drasi Authors.
# SPDX-License-Identifier: Apache-2.0

import asyncio
import unittest
from contextlib import ExitStack
from unittest.mock import AsyncMock, MagicMock, call, patch

import demo
import workflow as order_workflow
from temporalio.client import WorkflowExecutionStatus
from temporalio.service import RPCError, RPCStatusCode


def state(stage):
    return {
        "status": stage,
        "workflowStatus": "RUNNING",
        "events": [{
            "signal": stage,
            "diff": {
                "queryId": stage,
                "operation": "ADD",
                "sequenceId": 1,
                "after": {"orderId": demo.ORDER_ID, "status": stage},
            },
        }],
    }


class ConfirmationTests(unittest.IsolatedAsyncioTestCase):
    def command(self, stack, wait):
        db = MagicMock()
        db.execute.side_effect = lambda sql, *args: MagicMock(
            fetchone=MagicMock(return_value=(True,) if "pg_try_advisory_lock" in sql else ("paid",))
        )
        stack.enter_context(patch("demo.psycopg.connect")).return_value.__enter__.return_value = db
        stack.enter_context(patch("demo.Client.connect", new=AsyncMock(return_value=MagicMock())))
        stack.enter_context(patch("demo.progress", new=AsyncMock(return_value=state("placed"))))
        stack.enter_context(patch("demo.wait_for_stage", new=wait))
        stack.enter_context(patch("demo.drasi_status"))
        stack.enter_context(patch.dict("os.environ", {"TEMPORAL_ADDRESS": "unused:7233"}))
        stack.enter_context(patch("sys.argv", ["demo.py", "shipped"]))
        stack.enter_context(patch("builtins.print"))
        return db

    async def test_pending_stage_blocks_the_next_database_mutation(self):
        waiting = asyncio.Event()
        confirm = asyncio.Event()

        async def wait(handle, stage):
            if stage == "paid":
                waiting.set()
                await confirm.wait()
            return state(stage)

        with ExitStack() as stack:
            wait_mock = AsyncMock(side_effect=wait)
            db = self.command(stack, wait_mock)
            task = asyncio.create_task(demo.main())
            try:
                await asyncio.wait_for(waiting.wait(), timeout=2)
                self.assertFalse(task.done())
                self.assertFalse(any("UPDATE" in c.args[0] for c in db.execute.call_args_list))
                confirm.set()
                await asyncio.wait_for(task, timeout=2)
            finally:
                if not task.done():
                    task.cancel()
                    await asyncio.gather(task, return_exceptions=True)
            self.assertEqual([c.args[1] for c in wait_mock.await_args_list], ["paid", "shipped"])
            self.assertIn(
                call("UPDATE orders SET status = %s WHERE id = %s", ("shipped", demo.ORDER_ID)),
                db.execute.call_args_list,
            )

    async def test_unconfirmed_previous_stage_never_changes_the_database(self):
        with ExitStack() as stack:
            db = self.command(stack, AsyncMock(side_effect=TimeoutError("paid is still pending")))
            with self.assertRaisesRegex(TimeoutError, "paid is still pending"):
                await demo.main()
            self.assertFalse(any("UPDATE" in c.args[0] for c in db.execute.call_args_list))

    async def test_replacement_workflow_cannot_resume_a_later_database_stage(self):
        with ExitStack() as stack:
            wait = AsyncMock(return_value=state("paid"))
            db = self.command(stack, wait)
            stack.enter_context(patch("demo.progress", new=AsyncMock(return_value={
                "status": "awaiting_placement", "events": [], "workflowStatus": "RUNNING",
            })))
            stack.enter_context(patch("sys.argv", ["demo.py", "ready"]))
            with self.assertRaisesRegex(RuntimeError, "history.*missing"):
                await demo.main()
            wait.assert_not_awaited()
            self.assertFalse(any("UPDATE" in c.args[0] for c in db.execute.call_args_list))

    async def test_previous_signal_cannot_confirm_a_new_stage_with_the_same_sequence(self):
        status = {"queries": {"shipped": {"checkpoint": {"sequence": 1}}}}
        with (
            patch("demo.drasi_status", return_value=status),
            patch("demo.progress", new=AsyncMock(side_effect=[state("paid"), state("shipped")])) as read,
            patch("demo.asyncio.sleep", new=AsyncMock()),
        ):
            confirmed = await demo.wait_for_stage(MagicMock(), "shipped")
        self.assertEqual(read.await_count, 2)
        self.assertEqual(confirmed["status"], "shipped")

    async def test_matching_signal_still_waits_for_its_checkpoint(self):
        with (
            patch("demo.drasi_status", side_effect=[
                {"queries": {"paid": {"checkpoint": {"sequence": 0}}}},
                {"queries": {"paid": {"checkpoint": {"sequence": 1}}}},
            ]),
            patch("demo.progress", new=AsyncMock(return_value=state("paid"))) as read,
            patch("demo.asyncio.sleep", new=AsyncMock()),
        ):
            await demo.wait_for_stage(MagicMock(), "paid")
        self.assertEqual(read.await_count, 2)


class WorkerStartupTests(unittest.IsolatedAsyncioTestCase):
    async def test_retained_orders_reuse_the_workflow_without_starting_again(self):
        for status in (WorkflowExecutionStatus.RUNNING, WorkflowExecutionStatus.COMPLETED):
            with self.subTest(status=status), ExitStack() as stack:
                client = MagicMock()
                client.start_workflow = AsyncMock()
                client.get_workflow_handle.return_value.describe = AsyncMock(
                    return_value=MagicMock(status=status)
                )
                db = stack.enter_context(patch("psycopg.connect")).return_value.__enter__.return_value
                db.execute.return_value.fetchone.return_value = ("delivered",)
                stack.enter_context(patch("workflow.Client.connect", new=AsyncMock(return_value=client)))
                stack.enter_context(patch("workflow.Worker")).return_value = AsyncMock()
                stack.enter_context(patch("workflow.asyncio.Event")).return_value.wait = AsyncMock()
                stack.enter_context(patch.dict("os.environ", {"TEMPORAL_ADDRESS": "unused:7233"}))
                stack.enter_context(patch("builtins.print"))
                await order_workflow.main()
                client.start_workflow.assert_not_awaited()

    async def test_missing_history_never_restarts_an_existing_order(self):
        client = MagicMock()
        client.start_workflow = AsyncMock(side_effect=AssertionError("started a replacement workflow"))
        client.get_workflow_handle.return_value.describe = AsyncMock(side_effect=RPCError(
            "workflow not found", status=RPCStatusCode.NOT_FOUND, raw_grpc_status=b"",
        ))
        with ExitStack() as stack:
            db = stack.enter_context(patch("psycopg.connect")).return_value.__enter__.return_value
            db.execute.return_value.fetchone.return_value = ("delivered",)
            stack.enter_context(patch("workflow.Client.connect", new=AsyncMock(return_value=client)))
            stack.enter_context(patch("workflow.Worker")).return_value = AsyncMock()
            stack.enter_context(patch.dict("os.environ", {"TEMPORAL_ADDRESS": "unused:7233"}))
            with self.assertRaisesRegex(RuntimeError, "history.*missing"):
                await order_workflow.main()
        client.start_workflow.assert_not_awaited()


if __name__ == "__main__":
    unittest.main()
