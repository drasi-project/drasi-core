#!/usr/bin/env python3
"""Verify framework metrics, reaction handling logs, and signal policies."""

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
    require(status["service"] == "drasi-observability-demo", "wrong service identity")
    config = status["config"]
    require(status["sampling"] == sampling, "trace sampling mode differs")
    require("error" not in status["span_buffer"], "native collector reports an error")
    require(not any(status["span_losses"].values()), "native span collection lost records")
    require(not any(status["log_losses"].values()), "component logging lost records")
    metrics = fetch(url, "/metrics", False)
    logs = fetch(url, "/logs")
    source = config["source"]
    require(source == ("postgres" if config["scenario"] == "postgres" else "http"), "wrong source for selected demo")
    inactive = "http" if source == "postgres" else "postgres"
    require(not any(f'component_id="{inactive}' in line for line in metrics.splitlines()), "inactive source pipeline produced metric series")
    outputs = ["http-fast", "http-slow"] if config["scenario"] == "latency" else [f"{source}-output"]
    allowed = {source, f"{source}-query", *outputs}
    require(all(log["component"] in allowed for log in logs), "unexpected pipeline component produced logs")
    rows = 10 if config["scenario"] == "latency" else 1
    for component in outputs:
        handled = [log for log in logs if log["component"] == component and log["message"] == "Handled query result changes"]
        if config["log_level"] in ["off", "error", "warn"]:
            require(not handled, "info handling logs escaped the logging policy")
            continue
        require(len(handled) == 1, f"{component} handling log is missing or duplicated")
        log = handled[0]
        fields = dict(log["attributes"])
        require(fields.get("query_id") == {"String": f"{source}-query"}, "handling log lost query identity")
        require(fields.get("result_changes") == {"I64": rows}, "handling log has the wrong result change count")
        require(fields.get("added") == {"I64": rows}, "handling log has the wrong added count")
        require(fields.get("updated") == {"I64": 0} and fields.get("deleted") == {"I64": 0}, "handling log has incorrect operation counts")
        if config["tracing"]:
            require(re.fullmatch(r"[0-9a-f]{32}", log["trace_id"] or ""), "handling log lost its trace ID")
            require(re.fullmatch(r"[0-9a-f]{16}", log["span_id"] or ""), "handling log lost its span ID")
            require(log["sampled"] == (sampling == "always"), "handling log has wrong sampling state")
        else:
            require(log["trace_id"] is None and log["span_id"] is None, "disabled tracing produced event context")
    if not config["metrics"]:
        require(not any(line.startswith("drasi_") for line in metrics.splitlines()), "disabled metrics produced series")
    if config["metric_exclusion"] == "drasi.query":
        require(not any(line.startswith("drasi_query_") for line in metrics.splitlines()), "excluded query metrics produced series")
    elif config["metrics"] and not config["metric_exclusion"]:
        component = f"{source}-query"
        inputs = 11 if config["scenario"] == "latency" else 1
        require(sample(metrics, "drasi_source_events_dispatched", source, aggregate=True) == inputs, "source envelope count differs")
        require(sample(metrics, "drasi_query_events_processed", component, aggregate=True) == inputs, "framework input count differs")
        require(sample(metrics, "drasi_query_results_emitted", component, aggregate=True) == rows, "framework result-row count differs")
        require(sample(metrics, "drasi_query_engine_duration_count", component, aggregate=True) == inputs, "query evaluator timing count differs")
        for output in outputs:
            require(sample(metrics, "drasi_component_inputs_processed", output, aggregate=True) == 1, "reaction handling count differs")
        if config["profile"] == "persistence":
            require(sample(metrics, "drasi_query_commit_duration_count", component, aggregate=True) == inputs, "Persistence commit timing is missing")
            require(sample(metrics, "drasi_query_live_results", component) == rows, "Persistence live-result gauge differs")
        else:
            require(not any(line.startswith("drasi_query_commit_duration") for line in metrics.splitlines()), "non-Persistence profile recorded storage timing")
    return source


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:19091")
    parser.add_argument("--sampling", choices=["always", "never"], default="always")
    args = parser.parse_args()
    try:
        source = verify(args.url, args.sampling)
    except (OSError, URLError, ValueError, KeyError, TypeError) as error:
        parser.exit(1, f"Telemetry verification failed: {error}\n")
    print(f"PASS {source} telemetry: framework metrics and handling logs obey signal, sampling, exclusion, and logging policies; sampling={args.sampling}")


if __name__ == "__main__":
    main()
