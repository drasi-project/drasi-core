# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.

import importlib.util
from pathlib import Path
import sys
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "core_workloads", Path(__file__).parents[1] / "measure-core-workloads.py"
)
workloads = importlib.util.module_from_spec(spec)
spec.loader.exec_module(workloads)


class WorkloadGuards(unittest.TestCase):
    def setUp(self):
        self.case = {
            "execution": "native", "runtime": "current-thread", "workload": "aggregate",
            "queries": 4, "payload_bytes": 4096, "window": 32, "workers": 1,
        }
        self.value = {
            **self.case, "events": 100, "verified_results": 400,
            **dict.fromkeys(workloads.METRICS, 10),
        }
        self.value["effective_cpu_cores"] = 1.0

    def test_every_axis_and_result_count_is_verified(self):
        workloads.validate_result(self.value, self.case, 100)
        for key in [*self.case, "events", "verified_results"]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                workloads.validate_result({**self.value, key: "wrong"}, self.case, 100)

    def test_missing_nonfinite_and_zero_measurements_fail(self):
        for metric in workloads.METRICS:
            for value in [None, 0, -1, float("inf"), float("nan"), "10", True]:
                with self.subTest(metric=metric, value=value), self.assertRaises(ValueError):
                    workloads.validate_result({**self.value, metric: value}, self.case, 100)

    def test_summary_preserves_range_instead_of_only_reporting_best_run(self):
        runs = [dict.fromkeys(workloads.METRICS, value) for value in [3, 1, 20]]
        for value in workloads.summarize(runs).values():
            self.assertEqual(value, {"median": 3, "min": 1, "max": 20})

    def test_worker_axis_does_not_repeat_current_thread_or_omit_multicore_cases(self):
        cases = workloads.cases_for_workers(
            {"runtime": ["current-thread", "multi-thread"], "queries": [1, 4]},
            [1, 2, 4],
        )
        self.assertEqual(len(cases), 8)
        for queries in [1, 4]:
            self.assertEqual(
                [case["workers"] for case in cases if case["queries"] == queries and case["runtime"] == "current-thread"],
                [1],
            )
            self.assertEqual(
                [case["workers"] for case in cases if case["queries"] == queries and case["runtime"] == "multi-thread"],
                [1, 2, 4],
            )

    def test_cpu_utilization_cannot_mix_steady_state_time_with_full_process_cpu(self):
        with self.assertRaises(ValueError):
            workloads.validate_result(
                {**self.value, "effective_cpu_cores": 3.0},
                self.case,
                100,
            )


if __name__ == "__main__":
    unittest.main()
