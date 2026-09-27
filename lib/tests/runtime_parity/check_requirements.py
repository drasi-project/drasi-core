#!/usr/bin/env python3
# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.
"""Validate the requirement ledger and bind it to actual runner evidence."""

import argparse
import csv
from datetime import date
import json
from pathlib import Path


FIELDS = ("id", "priority", "state", "scope", "profile", "package", "binary", "case", "remaining")
EXPECTED_IDS = {
    f"{prefix}{number:02}"
    for prefix, count in (("M", 7), ("G", 6), ("Q", 8), ("T", 7), ("S", 5), ("R", 5), ("A", 5), ("H", 5))
    for number in range(1, count + 1)
}


def load_ledger(path):
    rows = []
    with path.open() as stream:
        for values in csv.reader((line for line in stream if line.strip() and not line.startswith("#")), delimiter="\t"):
            if len(values) != len(FIELDS) or any(not value for value in values):
                raise ValueError(f"invalid requirement row: {values}")
            row = dict(zip(FIELDS, values))
            if row["priority"] not in ("P1", "P2") or row["state"] not in ("covered", "partial", "blocked", "unqualified"):
                raise ValueError(f"invalid priority/state for {row['id']}")
            executable = all(row[key] != "-" for key in ("profile", "package", "binary", "case"))
            if executable != (row["state"] in ("covered", "partial")):
                raise ValueError(f"{row['id']} needs an explicit executable case or an explicit qualification gap")
            if (row["remaining"] == "-") != (row["state"] == "covered"):
                raise ValueError(f"{row['id']} has inconsistent qualification/gap metadata")
            rows.append(row)
    ids = [row["id"] for row in rows]
    if set(ids) != EXPECTED_IDS or len(ids) != len(EXPECTED_IDS):
        raise ValueError("ledger must contain every approved scenario exactly once (M01..H05)")
    return rows


def cases(path, executed=False):
    result = {}
    with path.open() as stream:
        for line in stream:
            fields = line.rstrip("\n").split("\t")
            if len(fields) != (3 if executed else 2):
                raise ValueError(f"invalid runner evidence in {path}: {line!r}")
            key = (fields[0], fields[1])
            if key in result:
                raise ValueError(f"duplicate runner evidence: {key}")
            result[key] = fields[2] if executed else "discovered"
    return result


def check_exemptions(path, today=None):
    today = today or date.today()
    with path.open() as stream:
        for row in csv.reader((line for line in stream if line.strip() and not line.startswith("#")), delimiter="\t"):
            if len(row) != 7 or any(not field for field in row):
                raise ValueError(f"invalid ignored-test exception: {row}")
            if row[3] == "-" and row[5] != "diagnostic" and row[6] == "-":
                raise ValueError(f"quarantine requires an expiry: {row[2]}")
            if row[6] != "-" and date.fromisoformat(row[6]) < today:
                raise ValueError(f"expired test quarantine: {row[2]} (owner {row[5]}, expired {row[6]})")


def check_discovery(rows, profile, package, discovered):
    missing = [
        f"{row['id']}: {row['binary']}::{row['case']}"
        for row in rows
        if row["profile"] in (profile, "all") and row["package"] == package
        and (row["binary"], row["case"]) not in discovered
    ]
    if missing:
        raise ValueError("missing requirement evidence: " + "; ".join(missing))


def report(rows, directory):
    outcomes = {}
    for profile in {row["profile"] for row in rows} - {"-"}:
        path = directory / f"{profile}.executed.tsv"
        outcomes[profile] = cases(path, executed=True) if path.exists() else {}
    requirements = []
    for row in rows:
        outcome = outcomes.get(row["profile"], {}).get((row["binary"], row["case"]), "not-run")
        requirements.append({
            **row,
            "outcome": outcome,
            "satisfied": row["state"] == "covered" and outcome == "ok",
        })
    matrix = directory / "matrix.exit-code"
    mode = directory / "execution-mode"
    matrix_passed = matrix.exists() and matrix.read_text().strip() == "0"
    actual_execution = mode.exists() and mode.read_text().strip() == "actual"
    return {
        "schema_version": 1,
        "test_matrix_passed": matrix_passed,
        "actual_execution": actual_execution,
        "replacement_qualified": matrix_passed and actual_execution and all(row["satisfied"] for row in requirements),
        "requirements": requirements,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ledger", type=Path, default=Path(__file__).with_name("requirements.tsv"))
    parser.add_argument("--profile")
    parser.add_argument("--package")
    parser.add_argument("--discovered", type=Path)
    parser.add_argument("--evidence-dir", type=Path)
    parser.add_argument("--require-qualified", action="store_true")
    args = parser.parse_args()
    if any((args.profile, args.package, args.discovered)) and not all((args.profile, args.package, args.discovered)):
        parser.error("--profile, --package and --discovered must be supplied together")
    if args.require_qualified and not args.evidence_dir:
        parser.error("--require-qualified requires --evidence-dir")
    try:
        rows = load_ledger(args.ledger)
        check_exemptions(Path(__file__).with_name("allowed-ignored.tsv"))
        if args.profile:
            check_discovery(rows, args.profile, args.package, cases(args.discovered))
        if args.evidence_dir:
            result = report(rows, args.evidence_dir)
            destination = args.evidence_dir / "qualification.json"
            destination.write_text(json.dumps(result, indent=2) + "\n")
            satisfied = sum(row["satisfied"] for row in result["requirements"])
            print(f"Requirement evidence: {satisfied}/{len(rows)} satisfied; replacement_qualified={result['replacement_qualified']}. Report: {destination}")
            for row in result["requirements"]:
                if not row["satisfied"]:
                    print(f"  {row['id']} [{row['state']}; {row['outcome']}]: {row['remaining']}")
            if args.require_qualified and not result["replacement_qualified"]:
                return 1
        return 0
    except (OSError, ValueError) as error:
        print(f"Requirement validation failed: {error}")
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
