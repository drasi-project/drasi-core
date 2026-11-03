#!/usr/bin/env python3
# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.

"""Check independently selected native builds, not a feature-unified workspace."""

import json
import subprocess
import sys

FORBIDDEN = {
    "drasi-lib",
    "drasi-core",
    "drasi-plugin-sdk",
    "drasi-query-cypher",
    "drasi-query-gql",
    "drasi-functions-cypher",
    "drasi-functions-gql",
}


def cargo(*arguments):
    result = subprocess.run(["cargo", *arguments], text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(f"cargo {' '.join(arguments)} failed:\n{result.stderr}")
    return result.stdout


def forbidden_dependencies(tree):
    return FORBIDDEN.intersection(
        line.split()[0] for line in tree.splitlines() if line.split()
    )


def dependency_selections(native):
    packages = ["drasi-core-types", "drasi-computation-contracts", "drasi-computation-plugin-sdk", *native]
    for package in packages:
        yield package, ["dynamic-plugin"] if package in native else []
    yield "drasi-computation-plugin-sdk", ["otel-context"]


def main():
    metadata = json.loads(cargo("metadata", "--locked", "--no-deps", "--format-version", "1"))
    native = sorted(
        package["name"]
        for package in metadata["packages"]
        if (package.get("metadata") or {}).get("drasi-plugin", {}).get("abi-family") == "computation"
    )
    if not native:
        raise RuntimeError("No native plugin declarations were found; isolation was not checked.")
    failed = []
    for package, features in dependency_selections(native):
        arguments = ["tree", "--locked", "-p", package, "--edges", "normal,build", "--prefix", "none", "--format", "{p}"]
        if features:
            arguments += ["--features", ",".join(features)]
        label = f"{package} [{','.join(features)}]" if features else package
        unwanted = forbidden_dependencies(cargo(*arguments))
        if unwanted:
            failed.append(f"{label}: {', '.join(sorted(unwanted))}")
        else:
            print(f"{label}: no runtime, evaluator, query parser or legacy SDK dependency")
    if failed:
        raise RuntimeError("Native dependency isolation failed:\n" + "\n".join(failed))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError) as error:
        print(error, file=sys.stderr)
        sys.exit(1)
