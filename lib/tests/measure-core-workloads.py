#!/usr/bin/env python3
# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.
"""Measure current in-process core workloads without loading or changing plugins."""

import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import itertools
import json
from pathlib import Path
import platform
import statistics
import sys

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "fast_path_measurement", Path(__file__).with_name("measure-fast-path.py")
)
measurement = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measurement)

METRICS = (
    "events_per_second", "query_evaluations_per_second",
    "latency_p50_ns", "latency_p99_ns", "process_cpu_seconds",
    "max_rss_bytes", "shutdown_ns",
)


def validate_result(value, case, events):
    for key, expected in {**case, "events": events, "verified_results": events * case["queries"]}.items():
        if value.get(key) != expected:
            raise ValueError(f"Benchmark mismatch for {key}: expected {expected}, got {value.get(key)}")
    for metric in METRICS:
        number = value.get(metric)
        if isinstance(number, bool) or not isinstance(number, (int, float)) or not 0 < number < float("inf"):
            raise ValueError(f"Invalid measurement for {metric}: {number}")


def summarize(runs):
    return {
        metric: {
            "median": statistics.median(run[metric] for run in runs),
            "min": min(run[metric] for run in runs),
            "max": max(run[metric] for run in runs),
        }
        for metric in METRICS
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--events", type=int, default=10_000)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--queries", type=int, nargs="+", default=[1, 4])
    parser.add_argument("--windows", type=int, nargs="+", default=[1, 32])
    parser.add_argument("--payload-bytes", type=int, nargs="+", default=[0, 4096])
    parser.add_argument("--executions", nargs="+", choices=["ordinary", "native"], default=["ordinary", "native"])
    parser.add_argument("--runtimes", nargs="+", choices=["current-thread", "multi-thread"], default=["current-thread", "multi-thread"])
    parser.add_argument("--workloads", nargs="+", choices=["projection", "aggregate", "join", "persistent-aggregate"], default=["projection", "aggregate"])
    args = parser.parse_args()
    if not 1 <= args.events <= 10_000_000 or args.repetitions < 3:
        parser.error("events must be 1..10000000; at least three repetitions are required")
    if any(not 1 <= value <= 64 for value in args.queries + args.windows):
        parser.error("queries and windows must be 1..64")
    if any(not 0 <= value <= 1_048_576 for value in args.payload_bytes):
        parser.error("payload bytes must be 0..1048576")
    if "persistent-aggregate" in args.workloads and args.executions != ["native"]:
        parser.error("persistent-aggregate requires --executions native and a computation-rocksdb-tests binary")
    axes = {
        "execution": args.executions, "runtime": args.runtimes,
        "workload": args.workloads, "queries": args.queries,
        "payload_bytes": args.payload_bytes, "window": args.windows,
    }
    if any(len(values) != len(set(values)) for values in axes.values()):
        parser.error("matrix axes must not contain duplicate values")
    cases = [dict(zip(axes, values)) for values in itertools.product(*axes.values())]
    binary = args.binary.resolve(strict=True)
    report = {
        "schema_version": 1,
        "complete": False,
        "started_at": datetime.now(timezone.utc).isoformat(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "binary": {"path": str(binary), "sha256": hashlib.sha256(binary.read_bytes()).hexdigest()},
        "events": args.events,
        "repetitions": args.repetitions,
        "axes": axes,
        "notes": [
            "Use a release binary; each isolated process warms 2048 events across 64 keys.",
            "Every query result is checked against an independent projection, running-sum or 64-key join oracle.",
            "Latency ends when all queries have returned the expected result for an input.",
            "Ordinary/native use the same queries and input, but different pipeline plumbing; not a scheduler-only comparison.",
            "Multi-thread uses two workers; native nodes still share one controller.",
            "Only persistent-aggregate uses real native RocksDB; other workloads are memory-only. No dynamic libraries or external services.",
            "Join mode seeds 64 reference nodes and validates each joined value and payload; persistent mode is a throughput workload, not a crash test.",
            "Timers and actual process-crash recovery are qualified separately, not inferred from throughput.",
            "CPU and peak RSS include startup/warmup/shutdown; allocation traffic is not measured.",
            "shutdown_ns measures completed shutdown, not cancellation of in-flight business work.",
            "Harness generation/verification is included; local measurements are not production capacity guarantees.",
        ],
        "runs": [],
        "summary": {},
    }
    for iteration in range(args.repetitions):
        ordered = cases if iteration % 2 == 0 else list(reversed(cases))
        for case in ordered:
            value = measurement.measure(
                binary, args.events, case["window"],
                arguments=[case["queries"], case["payload_bytes"], case["execution"], case["runtime"], case["workload"]],
                require_allocations=False,
            )
            validate_result(value, case, args.events)
            key = "/".join(str(case[axis]) for axis in axes)
            report["runs"].append({"case": key, "iteration": iteration, **value})
            report["summary"][key] = summarize(
                [run for run in report["runs"] if run["case"] == key]
            )
            args.output.write_text(json.dumps(report, indent=2) + "\n")
            print(f"{key} run={iteration + 1}: {value['events_per_second']:.0f} inputs/s", flush=True)
    report["complete"] = True
    report["completed_at"] = datetime.now(timezone.utc).isoformat()
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
