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
QUERY_PLUGIN = "drasi-computation-transformer-query"
QUERY_ENGINE = {
    "drasi-core",
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


def forbidden_dependencies(tree, package=None):
    forbidden = FORBIDDEN - QUERY_ENGINE if package == QUERY_PLUGIN else FORBIDDEN
    return forbidden.intersection(
        line.split()[0] for line in tree.splitlines() if line.split()
    )


def main():
    metadata = json.loads(cargo("metadata", "--locked", "--no-deps", "--format-version", "1"))
    native = sorted(
        package["name"]
        for package in metadata["packages"]
        if (package.get("metadata") or {}).get("drasi-plugin", {}).get("abi-family") == "computation"
    )
    if not native:
        raise RuntimeError("No native plugin declarations were found; isolation was not checked.")
    packages = ["drasi-core-types", "drasi-computation-contracts", "drasi-computation-plugin-sdk", *native]
    failed = []
    for package in packages:
        arguments = ["tree", "--locked", "-p", package, "--edges", "normal,build", "--prefix", "none", "--format", "{p}"]
        if package in native:
            arguments += ["--features", "dynamic-plugin"]
        unwanted = forbidden_dependencies(cargo(*arguments), package)
        if unwanted:
            failed.append(f"{package}: {', '.join(sorted(unwanted))}")
        else:
            print(f"{package}: native dependency policy satisfied")
    if failed:
        raise RuntimeError("Native dependency isolation failed:\n" + "\n".join(failed))


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError) as error:
        print(error, file=sys.stderr)
        sys.exit(1)
