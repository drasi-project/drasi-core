# Copyright 2026 The Drasi Authors.
# SPDX-License-Identifier: Apache-2.0

import asyncio
import unittest
from contextlib import ExitStack
from unittest.mock import AsyncMock, MagicMock, call, patch

import demo
import workflow as order_workflow
from temporalio.service import RPCError, RPCStatusCode


def state(stage):
    return {
        "status": stage,
        "workflowStatus": "RUNNING",
        "events": [{
            "stage": stage,
            "diff": {
                "queryId": stage,
                "operation": "ADD",
                "sequenceId": 1,
                "after": {"orderId": demo.ORDER_ID, "status": stage},
            },
        }],
    }


class ConfirmationTests(unittest.IsolatedAsyncioTestCase):
    def command(self, stack, wait, before="paid", action="shipped"):
        db = MagicMock()
        db.execute.side_effect = lambda sql, *args: MagicMock(
            fetchone=MagicMock(return_value=(True,) if "pg_try_advisory_lock" in sql
                              else (before,) if before is not None else None)
        )
        current = state(before) if before else None
        if before:
            current["events"] = [
                state(stage)["events"][0] for stage in demo.STAGES[:demo.STAGES.index(before) + 1]
            ]
        stack.enter_context(patch("demo.psycopg.connect")).return_value.__enter__.return_value = db
        stack.enter_context(patch("demo.psycopg.ClientCursor", autospec=True))
        stack.enter_context(patch("demo.Client.connect", new=AsyncMock(return_value=MagicMock())))
        stack.enter_context(patch("demo.progress_if_exists", new=AsyncMock(return_value=current)))
        stack.enter_context(patch("demo.wait_for_stage", new=wait))
        stack.enter_context(patch("demo.drasi_status"))
        stack.enter_context(patch.dict("os.environ", {"TEMPORAL_ADDRESS": "unused:7233"}))
        stack.enter_context(patch("sys.argv", ["demo.py", action]))
        output = stack.enter_context(patch("builtins.print"))
        return db, output

    async def test_stage_commands_print_the_executed_mutation(self):
        insert = "INSERT INTO orders (id, status) VALUES (%s, %s)"
        update = "UPDATE orders SET status = %s WHERE id = %s"
        cases = (
            (None, "placed", insert, (demo.ORDER_ID, "placed"),
             "INSERT INTO orders (id, status) VALUES ('1001', 'placed')"),
            ("placed", "paid", update, ("paid", demo.ORDER_ID),
             "UPDATE orders SET status = 'paid' WHERE id = '1001'"),
            ("paid", "shipped", update, ("shipped", demo.ORDER_ID),
             "UPDATE orders SET status = 'shipped' WHERE id = '1001'"),
            ("shipped", "delivered", update, ("delivered", demo.ORDER_ID),
             "UPDATE orders SET status = 'delivered' WHERE id = '1001'"),
        )
        for before, action, statement, parameters, rendered in cases:
            with self.subTest(action=action), ExitStack() as stack:
                wait = AsyncMock(side_effect=lambda handle, stage: state(stage))
                db, output = self.command(stack, wait, before, action)
                cursor = demo.psycopg.ClientCursor.return_value.__enter__.return_value
                cursor.mogrify.return_value = rendered
                await demo.main()
                demo.psycopg.ClientCursor.assert_called_once_with(db)
                cursor.mogrify.assert_called_once_with(statement, parameters)
                db.execute.assert_any_call(statement, parameters)
                output.assert_any_call(f"SQL:        {rendered};", flush=True)

    async def test_repeated_or_invalid_stage_does_not_print_a_mutation(self):
        for action in ("paid", "placed"):
            with self.subTest(action=action), ExitStack() as stack:
                db, output = self.command(
                    stack, AsyncMock(return_value=state("paid")), before="paid", action=action,
                )
                if action == "placed":
                    with self.assertRaisesRegex(ValueError, "Cannot move order"):
                        await demo.main()
                else:
                    await demo.main()
                demo.psycopg.ClientCursor.assert_not_called()
                self.assertFalse(any(c.args[0].startswith(("INSERT", "UPDATE"))
                                     for c in db.execute.call_args_list))
                self.assertFalse(any(c.args[0].startswith("SQL:") for c in output.call_args_list))

    async def test_ready_for_delivered_order_shows_reset_command(self):
        with ExitStack() as stack:
            db, output = self.command(
                stack, AsyncMock(return_value=state("delivered")),
                before="delivered", action="ready",
            )
            await demo.main()
            output.assert_any_call(
                "This order is delivered. Run ./demo.sh down, then ./demo.sh up to start again."
            )
            self.assertFalse(any(c.args[0].startswith(("INSERT", "UPDATE"))
                                 for c in db.execute.call_args_list))

    async def test_ready_before_placement_does_not_start_a_workflow(self):
        with ExitStack() as stack:
            db, output = self.command(
                stack, AsyncMock(), before=None, action="ready",
            )
            await demo.main()
            output.assert_any_call("Temporal: order-1001 is not started.")
            self.assertFalse(any(c.args[0].startswith(("INSERT", "UPDATE"))
                                 for c in db.execute.call_args_list))

    async def test_ready_rejects_workflow_without_a_database_order(self):
        with ExitStack() as stack:
            db, _ = self.command(stack, AsyncMock(), before=None, action="ready")
            stack.enter_context(patch(
                "demo.progress_if_exists", new=AsyncMock(return_value=state("placed"))
            ))
            with self.assertRaisesRegex(RuntimeError, "database and workflow do not match"):
                await demo.main()
            self.assertFalse(any(c.args[0].startswith(("INSERT", "UPDATE"))
                                 for c in db.execute.call_args_list))

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
            db, _ = self.command(stack, wait_mock)
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
            db, _ = self.command(stack, AsyncMock(side_effect=TimeoutError("paid is still pending")))
            with self.assertRaisesRegex(TimeoutError, "paid is still pending"):
                await demo.main()
            self.assertFalse(any("UPDATE" in c.args[0] for c in db.execute.call_args_list))

    async def test_replacement_workflow_cannot_resume_a_later_database_stage(self):
        with ExitStack() as stack:
            wait = AsyncMock(return_value=state("paid"))
            db, _ = self.command(stack, wait, before="shipped", action="ready")
            stack.enter_context(patch("demo.progress_if_exists", new=AsyncMock(return_value={
                "status": "placed", "events": [state("placed")["events"][0]], "workflowStatus": "RUNNING",
            })))
            with self.assertRaisesRegex(RuntimeError, "history.*missing"):
                await demo.main()
            wait.assert_not_awaited()
            self.assertFalse(any("UPDATE" in c.args[0] for c in db.execute.call_args_list))

    async def test_previous_signal_cannot_confirm_a_new_stage_with_the_same_sequence(self):
        status = {"queries": {"shipped": {"checkpoint": {"sequence": 1}}}}
        with (
            patch("demo.drasi_status", return_value=status),
            patch("demo.progress_if_exists", new=AsyncMock(side_effect=[state("paid"), state("shipped")])) as read,
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
            patch("demo.progress_if_exists", new=AsyncMock(return_value=state("paid"))) as read,
            patch("demo.asyncio.sleep", new=AsyncMock()),
        ):
            await demo.wait_for_stage(MagicMock(), "paid")
        self.assertEqual(read.await_count, 2)

    async def test_unstarted_workflow_waits_for_pending_start(self):
        with (
            patch("demo.drasi_status", side_effect=[
                {"queries": {"placed": {"checkpoint": {"sequence": 0}}}},
                {"queries": {"placed": {"checkpoint": {"sequence": 1}}}},
            ]),
            patch("demo.progress_if_exists", new=AsyncMock(side_effect=[None, state("placed")])) as read,
            patch("demo.asyncio.sleep", new=AsyncMock()),
        ):
            confirmed = await demo.wait_for_stage(MagicMock(), "placed")
        self.assertEqual(read.await_count, 2)
        self.assertEqual(confirmed["status"], "placed")

    async def test_missing_acknowledged_workflow_is_not_recreated(self):
        with (
            patch("demo.drasi_status", return_value={"queries": {"placed": {"checkpoint": {"sequence": 1}}}}),
            patch("demo.progress_if_exists", new=AsyncMock(return_value=None)),
        ):
            with self.assertRaisesRegex(RuntimeError, "history is missing"):
                await demo.wait_for_stage(MagicMock(), "placed")

    async def test_progress_only_treats_not_found_as_unstarted(self):
        for code in (RPCStatusCode.NOT_FOUND, RPCStatusCode.UNAVAILABLE):
            with self.subTest(code=code), patch("demo.progress", new=AsyncMock(side_effect=RPCError(
                "failed", status=code, raw_grpc_status=b"",
            ))):
                if code == RPCStatusCode.NOT_FOUND:
                    self.assertIsNone(await demo.progress_if_exists(MagicMock()))
                else:
                    with self.assertRaises(RPCError):
                        await demo.progress_if_exists(MagicMock())


class WorkerStartupTests(unittest.IsolatedAsyncioTestCase):
    async def test_start_payload_precedes_already_buffered_signals(self):
        order = order_workflow.OrderLifecycle()
        for stage in ("paid", "shipped", "delivered"):
            getattr(order, stage)({"stage": stage})
        placed = {"queryId": "placed"}

        async def wait(condition):
            self.assertTrue(condition())

        with patch("workflow.workflow.wait_condition", new=AsyncMock(side_effect=wait)):
            result = await order.run(placed)
        self.assertEqual([event["stage"] for event in result["events"]],
                         ["placed", "paid", "shipped", "delivered"])
        self.assertEqual(result["events"][0]["diff"], placed)
        self.assertEqual(result["status"], "delivered")

    async def test_worker_registers_workflow_without_starting_it(self):
        client = MagicMock()
        client.start_workflow = AsyncMock()
        with ExitStack() as stack:
            stack.enter_context(patch("workflow.Client.connect", new=AsyncMock(return_value=client)))
            worker = stack.enter_context(patch("workflow.Worker")).return_value
            worker.run = AsyncMock()
            stack.enter_context(patch.dict("os.environ", {"TEMPORAL_ADDRESS": "unused:7233"}))
            stack.enter_context(patch("builtins.print"))
            await order_workflow.main()
        worker.run.assert_awaited_once()
        client.start_workflow.assert_not_awaited()

    async def test_health_checks_worker_polling_without_a_workflow(self):
        for pollers in ([MagicMock()], []):
            with self.subTest(pollers=pollers), ExitStack() as stack:
                client = MagicMock()
                client.workflow_service.describe_task_queue = AsyncMock(
                    return_value=MagicMock(pollers=pollers)
                )
                stack.enter_context(patch("demo.Client.connect", new=AsyncMock(return_value=client)))
                stack.enter_context(patch.dict("os.environ", {"TEMPORAL_ADDRESS": "unused:7233"}))
                stack.enter_context(patch("sys.argv", ["demo.py", "health"]))
                if pollers:
                    await demo.main()
                else:
                    with self.assertRaisesRegex(RuntimeError, "not polling"):
                        await demo.main()
                client.get_workflow_handle.return_value.describe.assert_not_called()


if __name__ == "__main__":
    unittest.main()
