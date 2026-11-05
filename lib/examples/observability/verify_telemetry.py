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


def sample(metrics, name, component):
    found = []
    for line in metrics.splitlines():
        if line.startswith(f"{name}{{") and f'component_id="{component}"' in line:
            found.append(float(line.rsplit(" ", 1)[1]))
    require(len(found) == 1, f"expected one scoped {name} sample for {component}")
    return found[0]


def verify(url, sampling):
    status = fetch(url, "/status")
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
        require(sample(metrics, f"{prefix}_record_bytes_inspected", component) > 0, f"{source} inspected no bytes")
        require(sample(metrics, f"{prefix}_inspection_duration_count", component) == 1, f"{source} inspection timing is not recorded once")
        require(sample(metrics, f"{prefix}_inspection_active", component) == 0, f"{source} inspection gauge did not clear")
        inspected = [log for log in logs if log["component"] == component and log["message"] == "Inspected serialized record images"]
        require(len(inspected) == 1, f"{source} native log is missing or duplicated")
        log = inspected[0]
        require(re.fullmatch(r"[0-9a-f]{32}", log["trace_id"] or ""), "native log lost its trace ID")
        require(re.fullmatch(r"[0-9a-f]{16}", log["span_id"] or ""), "native log lost its span ID")
        require(log["sampled"] == (sampling == "always"), "native log has wrong sampling state")
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
    print(f"PASS native telemetry: both pipelines expose {prefix}_* metrics and correlated logs; sampling={args.sampling}")


if __name__ == "__main__":
    main()
