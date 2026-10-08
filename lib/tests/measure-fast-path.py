#!/usr/bin/env python3
# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.
"""Compare identical release fast_path binaries with interleaved isolated runs."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import resource
import statistics
import subprocess


def measure(binary, events, window, plugin=None):
    environment = {**os.environ, "RUST_LOG": "error"}
    if plugin is not None:
        environment["DRASI_NATIVE_STANDARD_PLUGIN"] = str(plugin)
    system = platform.system()
    if system == "Darwin":
        command = ["/usr/bin/time", "-l", str(binary), str(events), str(window)]
        pattern = r"^\s*(\d+)\s+maximum resident set size"
        scale = 1
    elif system == "Linux":
        command = ["/usr/bin/time", "-v", str(binary), str(events), str(window)]
        pattern = r"^\s*Maximum resident set size \(kbytes\):\s*(\d+)"
        scale = 1024
    else:
        raise RuntimeError("RSS measurement requires macOS or Linux")
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    result = subprocess.run(
        command, capture_output=True, text=True, env=environment, timeout=180
    )
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    if result.returncode:
        raise RuntimeError(f"{binary} failed:\n{result.stdout}\n{result.stderr}")
    value = json.loads(result.stdout.splitlines()[-1])
    rss = re.search(pattern, result.stderr, re.MULTILINE)
    if rss is None:
        raise RuntimeError(f"Missing RSS measurement:\n{result.stderr}")
    value["max_rss_bytes"] = int(rss.group(1)) * scale
    value["process_cpu_seconds"] = (
        after.ru_utime + after.ru_stime - before.ru_utime - before.ru_stime
    )
    value["allocations_per_event"] = value["allocations"] / events
    value["allocated_bytes_per_event"] = value["allocated_bytes"] / events
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("current", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--events", type=int, default=100_000)
    parser.add_argument("--repetitions", type=int, default=7)
    parser.add_argument("--baseline-plugin", type=Path)
    parser.add_argument("--current-plugin", type=Path)
    args = parser.parse_args()
    if args.events <= 0 or args.repetitions < 3:
        parser.error("positive events and at least three repetitions are required")
    if (args.baseline_plugin is None) != (args.current_plugin is None):
        parser.error("specify both native plugin paths or neither")
    binaries = {
        "baseline": args.baseline.resolve(strict=True),
        "current": args.current.resolve(strict=True),
    }
    plugins = {
        "baseline": args.baseline_plugin.resolve(strict=True) if args.baseline_plugin else None,
        "current": args.current_plugin.resolve(strict=True) if args.current_plugin else None,
    }
    report = {
        "platform": platform.platform(),
        "machine": platform.machine(),
        "binaries": {
            name: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
            for name, path in binaries.items()
        },
        "plugins": {
            name: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
            for name, path in plugins.items() if path is not None
        },
        "notes": [
            "Release binaries must use identical benchmark source and compiler settings.",
            "Each process warms 2048 events before steady-state counters.",
            "Allocation counts include reallocations; bytes are allocation traffic, not live memory.",
            "CPU and peak RSS include process startup, warmup and shutdown.",
            "Current-thread ordinary projection or native arithmetic; no recovery or persistent storage.",
            "Native allocation counters cover the host only; CPU/RSS include the plugin.",
            "Interleaved runs characterize local noise, not every production workload.",
        ],
        "runs": [],
        "summary": {},
    }
    for window in [1, 32]:
        for name, binary in binaries.items():
            measure(binary, 1_000, window, plugins[name])
        for iteration in range(args.repetitions):
            order = ["baseline", "current"] if iteration % 2 == 0 else ["current", "baseline"]
            for name in order:
                value = measure(binaries[name], args.events, window, plugins[name])
                report["runs"].append({"binary": name, "iteration": iteration, **value})
                args.output.write_text(json.dumps(report, indent=2) + "\n")
                print(
                    f"window={window} {name} run={iteration + 1}: "
                    f"{value['events_per_second']:.0f}/s "
                    f"{value['allocations_per_event']:.2f} allocations/event",
                    flush=True,
                )
        metrics = [
            "events_per_second", "latency_p50_ns", "latency_p99_ns",
            "allocations_per_event", "allocated_bytes_per_event",
            "process_cpu_seconds", "max_rss_bytes",
        ]
        summary = {}
        for metric in metrics:
            summary[metric] = {}
            for name in binaries:
                values = [
                    run[metric] for run in report["runs"]
                    if run["binary"] == name and run["window"] == window
                ]
                summary[metric][name] = {
                    "median": statistics.median(values),
                    "min": min(values),
                    "max": max(values),
                }
            baseline = summary[metric]["baseline"]["median"]
            summary[metric]["change_percent"] = (
                100 * (summary[metric]["current"]["median"] / baseline - 1)
            )
        report["summary"][str(window)] = summary
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report["summary"], indent=2))


if __name__ == "__main__":
    main()
