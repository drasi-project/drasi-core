#!/usr/bin/env python3
# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.
"""Report production-only foundation line coverage, without hiding uncovered lines."""

import argparse
from functools import cache
import json
from pathlib import Path
import re


FILES = (
    "data.rs", "ports.rs", "pipe.rs", "bounded_pipe.rs", "broadcast_pipe.rs",
    "ranked_pipe.rs", "retained_pipe.rs", "retained_store.rs", "qos_pipe.rs",
)
TEST_FUNCTION = re.compile(r"foundation_tests|5tests|test_envelope")
ROOT = Path(__file__).resolve().parents[3]


@cache
def canonical_path(filename):
    return str(Path(filename).resolve())


def read_lcov(text):
    files = {}
    current = None
    for line in text.splitlines():
        if line.startswith("SF:"):
            current = files.setdefault(canonical_path(line[3:]), {})
        elif line.startswith("DA:"):
            if current is None:
                raise ValueError("LCOV line data without a source file")
            number, count, *_ = line[3:].split(",")
            number, count = int(number), int(count)
            if number <= 0 or count < 0:
                raise ValueError("invalid LCOV line/count")
            current[number] = max(current.get(number, 0), count)
        elif line == "end_of_record":
            current = None
    return files


def production_limit(source, filename, functions):
    lines = source.splitlines()
    limit = next((i for i, line in enumerate(lines, 1) if line == "#[cfg(test)]"), len(lines) + 1)
    # These modules keep test-only helpers/modules at the end. Fail rather than
    # silently exclude production code if somebody changes that convention.
    for function in functions:
        if TEST_FUNCTION.search(function["name"]):
            continue
        for region in function["regions"]:
            origin = function["filenames"][region[5]]
            if canonical_path(origin) == filename and region[0] >= limit:
                raise ValueError(f"production function after test section in {filename}: {function['name']}")
    return limit


def report(coverage, lcov, root=ROOT):
    if len(coverage["data"]) != 1:
        raise ValueError("expected a single-architecture coverage export")
    functions = coverage["data"][0]["functions"]
    observed = read_lcov(lcov)
    result = []
    for name in FILES:
        path = (root / "lib/src/computation/v1" / name).resolve()
        filename = str(path)
        if filename not in observed:
            raise ValueError(f"missing coverage for {name}")
        limit = production_limit(path.read_text(), filename, functions)
        lines = {line: count for line, count in observed[filename].items() if line < limit}
        if not lines:
            raise ValueError(f"no production coverage data for {name}")
        uncovered = sorted(line for line, count in lines.items() if count == 0)
        result.append({
            "file": str(path.relative_to(root)),
            "instrumented_lines": len(lines),
            "covered_lines": len(lines) - len(uncovered),
            "line_percent": round(100 * (len(lines) - len(uncovered)) / len(lines), 2),
            "uncovered_lines": uncovered,
        })
    return {
        "scope": "production lines in nine schema, port and pipe modules; test-only tails excluded",
        "full_line_coverage": all(not item["uncovered_lines"] for item in result),
        "branch_coverage_measured": False,
        "files": result,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("coverage_json", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--require-full", action="store_true")
    args = parser.parse_args()
    result = report(json.loads(args.coverage_json.read_text()), args.coverage_json.with_suffix(".lcov").read_text())
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    for item in result["files"]:
        print(f"{item['file']}: {item['covered_lines']}/{item['instrumented_lines']} ({item['line_percent']}%)")
    if args.require_full and not result["full_line_coverage"]:
        raise SystemExit("Full line coverage is not established; inspect uncovered_lines in the report.")


if __name__ == "__main__":
    main()
