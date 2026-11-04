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


def verify(data, source, trace_id):
    spans = [
        span
        for resource in data["result"]["resourceSpans"]
        for scope in resource["scopeSpans"]
        for span in scope["spans"]
    ]
    expected = Counter({
        "source.publish": 1,
        "component.process": 1,
        "query.evaluate": 1,
        "query.commit": 1,
        "pipe.send": 2,
        "component.confirm_outputs": 1,
        "sink.handle": 1,
    })
    require(Counter(span["name"] for span in spans) == expected, "unexpected span counts")
    require({span["traceId"] for span in spans} == {trace_id}, "trace IDs differ")
    require(len({span["spanId"] for span in spans}) == 8, "span IDs must be unique")
    require(all(text(span, "outcome") == "success" for span in spans), "unsuccessful operation")
    require(
        all(int(span["endTimeUnixNano"]) >= int(span["startTimeUnixNano"]) for span in spans),
        "invalid span timing",
    )
    one = {span["name"]: span for span in spans if span["name"] != "pipe.send"}
    publication = one["source.publish"]
    processing = one["component.process"]
    handling = one["sink.handle"]
    require(not publication.get("parentSpanId"), "source publication must be the trace root")
    require(text(publication, "component_id") == source, "wrong source identity")
    require(text(processing, "component_id") == f"{source}-query", "wrong query identity")
    require(text(handling, "component_id") == f"{source}-output", "wrong reaction identity")
    require(processing.get("parentSpanId") == publication["spanId"], "query parent is not publication")
    for name in ["query.evaluate", "query.commit", "component.confirm_outputs", "sink.handle"]:
        require(one[name].get("parentSpanId") == processing["spanId"], f"wrong {name} parent")
    require(
        Counter(span.get("parentSpanId") for span in spans if span["name"] == "pipe.send")
        == Counter([publication["spanId"], processing["spanId"]]),
        "wrong pipe acceptance parents",
    )
    require(text(handling, "completion") == "handled", "sink only confirms acceptance")
    require(text(handling, "component_kind") == "reaction", "wrong sink semantic identity")
    count(publication, "output_count", 1)
    count(processing, "input_count", 1)
    count(processing, "output_count", 1)
    count(one["query.evaluate"], "change_count", 1)
    count(one["query.evaluate"], "result_change_count", 1)
    count(handling, "input_count", 1)
    return len(spans)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", choices=["http", "postgres"])
    parser.add_argument("trace_id", help="trace_id printed by the handled callback")
    parser.add_argument("--jaeger", default="http://127.0.0.1:16689")
    parser.add_argument("--file", type=Path, help="verify an archived Jaeger v3 response instead")
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
        total = verify(data, args.source, trace_id)
    except (OSError, URLError, ValueError, KeyError) as error:
        parser.exit(1, f"Trace verification failed: {error}\n")
    print(f"PASS {args.source}: {total} spans, one trace, correct parentage/counts, completion=handled")
    print(f"Archive: {args.file}" if args.file else f"{args.jaeger}/trace/{trace_id}")


if __name__ == "__main__":
    main()
