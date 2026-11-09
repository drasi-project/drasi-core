#!/usr/bin/env python3
"""Verify a real demo trace retrieved from Jaeger's v3 query API."""

import argparse
from collections import Counter
import json
from pathlib import Path
import re
from urllib.error import URLError
from urllib.request import urlopen


def require(condition, message):
    if not condition:
        raise ValueError(message)


def attributes(span):
    return {entry["key"]: entry["value"] for entry in span.get("attributes", [])}


def text(span, key):
    return attributes(span).get(key, {}).get("stringValue")


def count(span, key, expected):
    value = attributes(span).get(key, {})
    require("intValue" in value, f"{span['name']}.{key} must be an integer attribute")
    require(int(value["intValue"]) == expected, f"{span['name']}.{key} != {expected}")


def verify(data, source, trace_id, profile="debug", scenario="follow"):
    for resource in data["result"]["resourceSpans"]:
        identity = {entry["key"]: entry["value"] for entry in resource["resource"].get("attributes", [])}
        require(identity.get("service.name", {}).get("stringValue") == "drasi-observability-demo", "wrong exported service.name")
    spans = [
        span
        for resource in data["result"]["resourceSpans"]
        for scope in resource["scopeSpans"]
        for span in scope["spans"]
    ]
    latency = source == "http" and scenario == "latency"
    rows = 10 if latency else 1
    outputs = ["http-fast", "http-slow"] if latency else [f"{source}-output"]
    expected = Counter({
        "source.publish": 1,
        "component.process": 1,
        "query.evaluate": 1,
        "query.commit": 1,
        "pipe.send": 1 + len(outputs),
        "component.confirm_outputs": 1,
        "sink.handle": len(outputs),
    })
    if profile != "debug":
        for name in ["query.evaluate", "query.commit", "pipe.send", "component.confirm_outputs"]:
            del expected[name]
    actual = Counter(span["name"] for span in spans)
    require(actual == expected, f"unexpected span counts: observed {dict(actual)}, expected {dict(expected)}")
    require({span["traceId"] for span in spans} == {trace_id}, "trace IDs differ")
    require(len({span["spanId"] for span in spans}) == sum(expected.values()), "span IDs must be unique")
    require(all(text(span, "outcome") == "success" for span in spans), "unsuccessful operation")
    require(
        all(int(span["endTimeUnixNano"]) >= int(span["startTimeUnixNano"]) for span in spans),
        "invalid span timing",
    )
    one = {span["name"]: span for span in spans if span["name"] not in ["pipe.send", "component.process", "component.confirm_outputs"]}
    publication = one["source.publish"]
    def component_span(name, component):
        matching = [span for span in spans if span["name"] == name and text(span, "component_id") == component]
        require(len(matching) == 1, f"expected one {name} for {component}")
        return matching[0]
    processing = component_span("component.process", f"{source}-query")
    handlers = [component_span("sink.handle", component) for component in outputs]
    require(not publication.get("parentSpanId"), "source publication must be the trace root")
    require(text(publication, "component_id") == source, "wrong source identity")
    require(text(processing, "component_id") == f"{source}-query", "wrong query identity")
    require(processing.get("parentSpanId") == publication["spanId"], "query parent is not publication")
    if profile == "debug":
        for name in ["query.evaluate", "query.commit"]:
            require(one[name].get("parentSpanId") == processing["spanId"], f"wrong {name} parent")
    if profile == "debug":
        require(component_span("component.confirm_outputs", f"{source}-query").get("parentSpanId") == processing["spanId"], "wrong query confirmation parent")
        require(
            Counter(span.get("parentSpanId") for span in spans if span["name"] == "pipe.send")
            == Counter([publication["spanId"]] + [processing["spanId"]] * len(outputs)),
            "wrong pipe acceptance parents",
        )
    for handling in handlers:
        require(handling.get("parentSpanId") == processing["spanId"], "reaction handling did not inherit query output context")
        require(text(handling, "completion") == "handled", "sink only confirms acceptance")
        require(text(handling, "component_kind") == "reaction", "wrong sink semantic identity")
        count(handling, "input_count", 1)
        count(handling, "change_count", rows)
    count(publication, "output_count", 1)
    count(processing, "input_count", 1)
    count(processing, "output_count", 1)
    if profile == "debug":
        count(one["query.evaluate"], "change_count", 1)
        count(one["query.evaluate"], "result_change_count", rows)
    return len(spans)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", choices=["http", "postgres"])
    parser.add_argument("trace_id", help="trace_id printed by the handled callback")
    parser.add_argument("--jaeger", default="http://127.0.0.1:16689")
    parser.add_argument("--file", type=Path, help="verify an archived Jaeger v3 response instead")
    parser.add_argument("--profile", choices=["basic", "debug", "persistence"], default="debug")
    parser.add_argument("--scenario", choices=["follow", "controls", "latency", "postgres"], default="follow")
    args = parser.parse_args()
    trace_id = args.trace_id.lower()
    if not re.fullmatch(r"[0-9a-f]{32}", trace_id):
        parser.error("trace_id must contain 32 hexadecimal characters")
    try:
        if args.file:
            with args.file.open(encoding="utf-8") as response:
                data = json.load(response)
        else:
            with urlopen(f"{args.jaeger}/api/v3/traces/{trace_id}", timeout=10) as response:
                data = json.load(response)
        total = verify(data, args.source, trace_id, args.profile, args.scenario)
    except (OSError, URLError, ValueError, KeyError) as error:
        parser.exit(1, f"Trace verification failed: {error}\n")
    print(f"PASS {args.source}: {total} spans, one trace, correct parentage/counts, completion=handled")
    print(f"Archive: {args.file}" if args.file else f"{args.jaeger}/trace/{trace_id}")


if __name__ == "__main__":
    main()
