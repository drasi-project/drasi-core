#!/usr/bin/env python3
"""Verify the enhanced demo's native metrics, log correlation, and capabilities."""

import argparse
import json
import re
from urllib.error import URLError
from urllib.request import urlopen


def require(condition, message):
    if not condition:
        raise ValueError(message)


def fetch(url, path, json_value=True):
    with urlopen(f"{url}{path}", timeout=5) as response:
        body = response.read().decode()
    return json.loads(body) if json_value else body


def sample(metrics, name, component, aggregate=False):
    found = []
    for line in metrics.splitlines():
        if line.startswith(f"{name}{{") and f'component_id="{component}"' in line:
            found.append(float(line.rsplit(" ", 1)[1]))
    require(bool(found) if aggregate else len(found) == 1, f"expected scoped {name} samples for {component}, found {len(found)}")
    return sum(found) if aggregate else found[0]


def verify(url, sampling):
    status = fetch(url, "/status")
    config = status["config"]
    require(status["plugin_capabilities"] & 15 == 15, "native telemetry capabilities are missing")
    require(status["sampling"] == sampling, "trace sampling mode differs")
    require("error" not in status["span_buffer"], "native collector reports an error")
    require(not any(status["span_losses"].values()), "native span collection lost records")
    require(not any(status["log_losses"].values()), "component logging lost records")
    metrics = fetch(url, "/metrics", False)
    logs = fetch(url, "/logs")
    prefix = "drasi_plugin_drasi_demo_inspector"
    for source in ["http", "postgres"]:
        component = f"{source}-inspect"
        for suffix, dotted, predicate in [
            ("record_bytes_inspected", "record_bytes_inspected", lambda value: value > 0),
            ("inspection_duration_count", "inspection.duration", lambda value: value == 1),
            ("inspection_active", "inspection_active", lambda value: value == 0),
        ]:
            name = f"{prefix}_{suffix}"
            excluded = config["metric_exclusion"] and config["metric_exclusion"] in f"drasi.plugin.drasi-demo-inspector.{dotted}"
            if config["metrics"] and not excluded:
                require(predicate(sample(metrics, name, component)), f"{source} has incorrect {suffix}")
            else:
                require(not any(line.startswith(f"{name}{{") for line in metrics.splitlines()), f"disabled/excluded {name} was recorded")
        inspected = [log for log in logs if log["component"] == component and log["message"] == "Inspected serialized record images"]
        if config["log_level"] in ["off", "error", "warn"]:
            require(not inspected, "info logs escaped the logging policy")
            continue
        require(len(inspected) == 1, f"{source} native log is missing or duplicated")
        log = inspected[0]
        if config["tracing"]:
            require(re.fullmatch(r"[0-9a-f]{32}", log["trace_id"] or ""), "native log lost its trace ID")
            require(re.fullmatch(r"[0-9a-f]{16}", log["span_id"] or ""), "native log lost its span ID")
            require(log["sampled"] == (sampling == "always"), "native log has wrong sampling state")
        else:
            require(log["trace_id"] is None and log["span_id"] is None, "disabled tracing produced event context")
    if not config["metrics"]:
        require(not any(line.startswith("drasi_") for line in metrics.splitlines()), "disabled metrics produced series")
    if config["metric_exclusion"] == "drasi.query":
        require(not any(line.startswith("drasi_query_") for line in metrics.splitlines()), "excluded query metrics produced series")
    elif config["metrics"] and not config["metric_exclusion"]:
        for source in ["http", "postgres"]:
            component = f"{source}-query"
            inputs = 11 if source == "http" and config["scenario"] == "latency" else 1
            rows = 10 if source == "http" and config["scenario"] == "latency" else 1
            require(sample(metrics, "drasi_query_events_processed", component, aggregate=True) == inputs, "framework input count differs")
            require(sample(metrics, "drasi_query_results_emitted", component, aggregate=True) == rows, "framework result-row count differs")
            if config["profile"] == "persistence":
                require(sample(metrics, "drasi_query_commit_duration_count", component, aggregate=True) == inputs, "Persistence commit timing is missing")
                require(sample(metrics, "drasi_query_live_results", component) == rows, "Persistence live-result gauge differs")
            else:
                require(not any(line.startswith("drasi_query_commit_duration") for line in metrics.splitlines()), "non-Persistence profile recorded storage timing")
    return prefix


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:19091")
    parser.add_argument("--sampling", choices=["always", "never"], default="always")
    args = parser.parse_args()
    try:
        prefix = verify(args.url, args.sampling)
    except (OSError, URLError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f"Telemetry verification failed: {error}\n")
    print(f"PASS native telemetry: both pipelines obey signal, sampling, exclusion, and logging policies; sampling={args.sampling}")


if __name__ == "__main__":
    main()
