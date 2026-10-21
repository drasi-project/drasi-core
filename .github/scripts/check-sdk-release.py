#!/usr/bin/env python3
"""Check SDK compatibility-line changes and their complete local release closure.

Requires Python 3.11+. Read-only except for the optional JSON report.
"""

import argparse
import json
import re
import subprocess
import tomllib
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path


SDK_PACKAGES = {"drasi-plugin-sdk", "drasi-host-sdk", "drasi-ffi-primitives"}
ABI_PATH = "components/plugin-sdk/src/ffi/metadata.rs"
PLUGIN_PREFIXES = ("source-", "reaction-", "bootstrap-", "identity-", "secret-store-")


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def line(version):
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)(?:[-+].*)?", version)
    if not match:
        raise ValueError(f"Invalid version: {version}")
    return tuple(int(x) for x in match.groups()[:2])


def abi_requires_package_line_change(old_sdk, old_abi, sdk, abi):
    return line(abi) != line(old_abi) and line(sdk) == line(old_sdk)


def registry_availability(package):
    """Check all existing tags, not just one architecture; never push or sign."""
    result = {"name": package["name"], "version": package["new"]}
    name, version = package["name"], package["new"]

    def get(url, headers=None):
        request = urllib.request.Request(
            url, headers={"User-Agent": "drasi-sdk-release-check", **(headers or {})}
        )
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.read().decode(), response.headers.get("Link")

    if package["crates_io"]:
        try:
            text, _ = get(f"https://index.crates.io/{name[:2]}/{name[2:4]}/{name}")
            result["crates_io_unused"] = all(
                json.loads(row)["vers"] != version for row in text.splitlines()
            )
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
            result["crates_io_unused"] = True
    if package["dynamic"]:
        stripped = name.removeprefix("drasi-")
        prefix = next(p for p in PLUGIN_PREFIXES if stripped.startswith(p))
        repository = f"drasi-project/{prefix[:-1]}/{stripped[len(prefix):]}"
        query = urllib.parse.urlencode({
            "service": "ghcr.io", "scope": f"repository:{repository}:pull"
        })
        try:
            text, _ = get(f"https://ghcr.io/token?{query}")
            token = json.loads(text)["token"]
            url = f"https://ghcr.io/v2/{repository}/tags/list?n=1000"
            tags = []
            while url:
                text, link = get(url, {"Authorization": f"Bearer {token}"})
                tags.extend(json.loads(text).get("tags") or [])
                next_url = re.search(r'<([^>]+)>;\s*rel="next"', link or "")
                url = urllib.parse.urljoin(url, next_url.group(1)) if next_url else None
                if url and not url.startswith("https://ghcr.io/v2/"):
                    raise ValueError(f"Unexpected registry pagination URL: {url}")
            result["oci_unused"] = not any(
                t == version or t.startswith(f"{version}-") for t in tags
            )
        except urllib.error.HTTPError as error:
            # A denied token cannot distinguish private from nonexistent repositories.
            result["oci_error"] = f"HTTP {error.code}; verify repository availability before publishing"
    return result


def read_tree(ref=None):
    paths = git("ls-tree", "-r", "--name-only", ref) if ref else git("ls-files")
    read = (lambda p: git("show", f"{ref}:{p}")) if ref else (lambda p: Path(p).read_text())
    manifests = {
        p: tomllib.loads(read(p))
        for p in paths.splitlines()
        if p == "Cargo.toml" or p.endswith("/Cargo.toml")
    }
    workspace = manifests["Cargo.toml"]["workspace"]
    packages = {}
    for member in workspace["members"]:
        path = f"{member}/Cargo.toml"
        manifest = manifests[path]
        package = dict(manifest["package"])
        if isinstance(package["version"], dict):
            package["version"] = workspace["package"]["version"]
        package["manifest"] = path
        stripped = package["name"].removeprefix("drasi-")
        package["dynamic"] = (
            "dynamic-plugin" in manifest.get("features", {})
            and stripped.startswith(PLUGIN_PREFIXES)
        )
        packages[package["name"]] = package
    abi = re.search(r'pub const FFI_SDK_VERSION: &str = "([^"]+)"', read(ABI_PATH)).group(1)
    config = tomllib.loads(read("release-plz.toml"))
    excluded = {
        p["name"] for p in config.get("package", [])
        if p.get("release") is False or p.get("publish") is False
    }
    return manifests, workspace, packages, abi, excluded


def dependencies(manifest):
    for section in ("dependencies", "build-dependencies", "dev-dependencies"):
        for name, value in manifest.get(section, {}).items():
            yield name, value, section
    for target in manifest.get("target", {}).values():
        yield from dependencies(target)


def resolved_dependency(name, value, workspace):
    if isinstance(value, dict) and value.get("workspace"):
        value = workspace["dependencies"][name]
    if isinstance(value, str):
        return name, value
    return value.get("package", name), value.get("version")


def release_closure(manifests, workspace, packages, seeds=SDK_PACKAGES):
    affected = set(seeds) | {n for n, p in packages.items() if p["dynamic"]}
    while True:
        dependents = {
            name for name, package in packages.items()
            if package.get("publish") is not False
            and any(
                section != "dev-dependencies"
                and resolved_dependency(n, v, workspace)[0] in affected
                for n, v, section in dependencies(manifests[package["manifest"]])
            )
        }
        if dependents <= affected:
            return affected
        affected |= dependents


def check(base, output=None, check_unused=False):
    old_manifests, old_workspace, old, old_abi, _ = read_tree(base)
    manifests, workspace, packages, abi, excluded = read_tree()
    errors = []
    sdk = packages["drasi-plugin-sdk"]["version"]
    previous_sdk = old["drasi-plugin-sdk"]["version"]
    changed_line = line(sdk) != line(previous_sdk)
    if abi_requires_package_line_change(previous_sdk, old_abi, sdk, abi):
        errors.append("An ABI major/minor change requires a new SDK package major/minor line (#989)")
    for name in SDK_PACKAGES:
        if packages[name]["version"] != sdk:
            errors.append(f"{name} must use the same package version as drasi-plugin-sdk ({sdk})")

    changed = {
        name for name in old
        if name in packages and old[name]["version"] != packages[name]["version"]
    }
    affected = (
        release_closure(old_manifests, old_workspace, old, changed | SDK_PACKAGES)
        if changed_line else set()
    )
    for name in sorted(affected):
        if name not in packages or packages[name]["version"] == old[name]["version"]:
            errors.append(f"{name} needs a new release version for SDK {sdk}")
        elif tuple(map(int, packages[name]["version"].split("."))) < tuple(
            map(int, old[name]["version"].split("."))
        ):
            errors.append(f"{name}: release versions must increase")

    # Check standalone examples too: release-plz only rewrites workspace members.
    for path, manifest in manifests.items():
        dependency_workspace = manifest.get("workspace", workspace)
        for name, value, _ in dependencies(manifest):
            name, requirement = resolved_dependency(name, value, dependency_workspace)
            if name in packages and requirement and name in affected:
                expected = packages[name]["version"]
                if requirement not in (expected, f"^{expected}", f"={expected}"):
                    errors.append(f"{path}: {name} requires {requirement}, expected {expected}")
    for name, value in workspace.get("dependencies", {}).items():
        name, requirement = resolved_dependency(name, value, workspace)
        if name in affected and requirement != packages[name]["version"]:
            errors.append(f"workspace dependency {name}: {requirement} != {packages[name]['version']}")

    # Cargo's normalized metadata checks inherited/path requirements and target declarations.
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"], text=True
    ))
    for package in metadata["packages"]:
        if packages[package["name"]]["dynamic"] and not any(
            "cdylib" in target["crate_types"] for target in package["targets"]
        ):
            errors.append(f"{package['name']}: dynamic-plugin is missing a cdylib target")

    report = {
        "base": git("rev-parse", base),
        "sdk": sdk,
        "ffi_abi": abi,
        "core": packages["drasi-core"]["version"],
        "lib": packages["drasi-lib"]["version"],
        "packages": [
            {
                "name": name,
                "old": old.get(name, {}).get("version"),
                "new": p["version"],
                "manifest": p["manifest"],
                "dynamic": p["dynamic"],
                "crates_io": p.get("publish") is not False and name not in excluded,
                "release_excluded": name in excluded,
            }
            for name, p in sorted(packages.items())
            if name in affected or p["version"] != old.get(name, {}).get("version")
        ],
        "errors": errors,
    }
    if check_unused:
        with ThreadPoolExecutor(max_workers=4) as executor:
            report["availability"] = list(executor.map(registry_availability, report["packages"]))
        for package in report["availability"]:
            if package.get("crates_io_unused") is False or package.get("oci_unused") is False:
                errors.append(f"{package['name']}@{package['version']} already exists")
            if "oci_error" in package:
                errors.append(f"{package['name']}: {package['oci_error']}")
    if output:
        Path(output).write_text(json.dumps(report, indent=2) + "\n")
    for error in errors:
        print(f"ERROR: {error}")
    print(
        f"SDK {previous_sdk} -> {sdk}; ABI {old_abi} -> {abi}; "
        f"{sum(p['dynamic'] for p in packages.values())} dynamic plugins; "
        f"{len(report['packages'])} changed packages; {len(errors)} errors"
    )
    return not errors


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True, help="Git base revision to compare")
    parser.add_argument("--write-map", help="Write a machine-readable version map")
    parser.add_argument("--check-unused", action="store_true", help="Read crates.io/GHCR availability")
    args = parser.parse_args()
    raise SystemExit(0 if check(args.base, args.write_map, args.check_unused) else 1)
