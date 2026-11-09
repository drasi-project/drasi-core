#!/usr/bin/env python3
"""Exercise real dashboard actions and verify framework metrics, not UI estimates."""

import argparse
import json
import re
import time
from urllib.request import Request, urlopen
from urllib.error import HTTPError

from verify_telemetry import fetch, require


def total(metrics, name, **labels):
    values = []
    for line in metrics.splitlines():
        if not line.startswith(f"{name}{{"):
            continue
        observed = dict(re.findall(r'(\w+)="([^"]*)"', line.split("}", 1)[0]))
        if all(observed.get(key) == value for key, value in labels.items()):
            values.append(float(line.rsplit(" ", 1)[1]))
    return sum(values)


def delta(metrics, baseline, name, **labels):
    return total(metrics, name, **labels) - total(baseline, name, **labels)


def check(state, metrics, phase):
    config = state["config"]
    baseline = state["baseline_metrics"]
    latency = config["scenario"] == "latency"
    source = config["source"]
    components = ["http-fast", "http-slow"] if latency else [f"{source}-output"]
    expected = 1 if phase == "paused-seed" else (4 if latency else 3)
    require(state["paused"] == phase.startswith("paused"), "wrong pause state")
    for component in components:
        observed = state["since_reset"].get(component, {})
        completed = 0 if phase.startswith("paused") and component == "http-slow" else expected
        require(observed.get("envelopes", 0) == completed, f"{component}: expected {completed} handled envelopes")
        if latency:
            require(observed.get("rows", 0) == completed * 10, f"{component}: expected ten rows per envelope")
            samples = delta(metrics, baseline, "drasi_pipeline_end_to_end_duration_count", component_id=component, query_id="http-query", phase="steady")
            require(samples == completed, f"{component}: latency must observe envelopes, not rows or acceptance")
    if config["metrics"] and not (config["metric_exclusion"] and config["metric_exclusion"] in "drasi.query.events_processed"):
        require(delta(metrics, baseline, "drasi_query_events_processed", component_id=f"{source}-query", query_id=f"{source}-query", phase="steady") == expected, "query input envelope count differs")
    if latency:
        require(delta(metrics, baseline, "drasi_query_results_emitted", component_id="http-query", query_id="http-query") == expected * 10, "query result-row count differs")
        if phase == "paused-burst":
            require(total(metrics, "drasi_queue_depth") >= 2, "slow branch did not retain queued work")
        if phase == "released":
            means = {}
            for component in components:
                means[component] = delta(metrics, baseline, "drasi_pipeline_end_to_end_duration_sum", component_id=component, query_id="http-query", phase="steady") / expected
            require(means["http-slow"] >= config["slow_delay_ms"] / 1000, "slow handling delay is missing from latency")
            require(means["http-slow"] > means["http-fast"], "slow branch did not have greater latency")
    else:
        require(total(metrics, "drasi_pipeline_end_to_end_duration_count") == 0, "default-off latency was accidentally enabled")


def action(url, name):
    request = Request(f"{url}/action", json.dumps({"action": name}).encode(), {"Content-Type": "application/json"})
    try:
        with urlopen(request, timeout=15) as response:
            return json.load(response)
    except HTTPError as error:
        raise ValueError(f"{name}: HTTP {error.code}: {error.read().decode()}") from error


def wait(url, phase):
    deadline = time.monotonic() + 10
    while True:
        try:
            check(fetch(url, "/scenario"), fetch(url, "/metrics", False), phase)
            return
        except (ValueError, KeyError) as error:
            if time.monotonic() >= deadline:
                raise ValueError(f"{phase}: {error}") from error
            time.sleep(0.1)


def exercise(url):
    state = fetch(url, "/scenario")
    require(state["config"]["source"] == "http", "PostgreSQL input must be exercised through SQL")
    latency = state["config"]["scenario"] == "latency"
    action(url, "reset")
    if latency:
        action(url, "pause")
        action(url, "burst")
        action(url, "insert")
        wait(url, "paused-burst")
        print("PASS paused burst: four inputs / forty result rows; fast handling continues, slow samples stay zero, queue depth grows")
        action(url, "release")
        wait(url, "released")
        print("PASS release: four latency samples per sink, not forty; slow latency includes actual handling delay")
    else:
        action(url, "insert")
        action(url, "update")
        action(url, "delete")
        wait(url, "actions")
        print("PASS follow/control actions: insert, update, delete reach the real handled callback; latency remains default-off")
    action(url, "reset")
    reset = fetch(url, "/scenario")
    require(reset["records"] == 0 and not reset["paused"], "reset did not clean up owned nodes/release handling")
    require(all(value["envelopes"] == 0 for value in reset["since_reset"].values()), "reset did not establish an observation baseline")
    print("PASS reset: owned HTTP nodes removed; cumulative instruments retained")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:19091")
    parser.add_argument("--check", choices=["paused-seed", "paused-burst", "released", "actions"])
    parser.add_argument("--exercise", action="store_true")
    args = parser.parse_args()
    try:
        if args.exercise:
            exercise(args.url)
        elif args.check:
            check(fetch(args.url, "/scenario"), fetch(args.url, "/metrics", False), args.check)
            print(f"PASS {args.check}: expected envelope, row, branch and queue observations")
        else:
            parser.error("select --check or --exercise")
    except (OSError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f"Scenario verification failed: {error}\n")


if __name__ == "__main__":
    main()
