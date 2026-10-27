#!/usr/bin/env python3
"""Check Bylaw inventories and production dependencies across all feature/target declarations."""

import json
from pathlib import Path
import subprocess
import sys
import tomllib


ROOT = Path(__file__).resolve().parents[2]
POLICY = ROOT / "bylaw.toml"
ORIGINS = {
    "ffi": {"drasi-ffi-primitives"},
    "runtime": {"drasi-lib"},
    "ast": {"drasi-query-ast"},
    "parsers": {"drasi-query-cypher", "drasi-query-gql"},
}
EVALUATION = {"drasi-core", "drasi-functions-cypher", "drasi-functions-gql", "drasi-lib"}
PLUGIN_DIRECTORIES = {"bootstrappers", "sources", "reactions", "identity", "secret_stores"}
PLUGIN_PREFIXES = tuple(
    f"drasi-{kind}-" for kind in ("source", "reaction", "bootstrap", "identity", "secret-store")
)
LIBRARY_KINDS = {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}
RULES = {
    "ffi-is-std-only": ("only-dependencies", "ffi", ["ffi"]),
    "runtime-does-not-depend-on-plugins": (
        "forbid-dependencies", "runtime", ["plugins", "external-plugins"],
    ),
    "runtime-does-not-depend-on-indexes": (
        "forbid-dependencies", "runtime", ["indexes", "external-indexes"],
    ),
    "ast-does-not-depend-on-upper-layers": (
        "forbid-dependencies", "ast",
        ["parsers", "external-parsers", "evaluation", "external-evaluation",
         "plugins", "external-plugins", "indexes", "external-indexes"],
    ),
    "parsers-do-not-depend-on-upper-layers": (
        "forbid-dependencies", "parsers",
        ["evaluation", "external-evaluation", "plugins", "external-plugins",
         "indexes", "external-indexes"],
    ),
}


def workspace_packages(metadata):
    members = set(metadata["workspace_members"])
    return [package for package in metadata["packages"] if package["id"] in members]


def is_concrete_plugin(name, component_directory=None):
    return name != "drasi-source-mapping" and (
        name.startswith(PLUGIN_PREFIXES) or component_directory in PLUGIN_DIRECTORIES
    )


def validate_policy(policy, metadata):
    errors = []
    root = Path(metadata["workspace_root"])
    packages = workspace_packages(metadata)
    names = {package["name"] for package in packages}
    origin_names = set().union(*ORIGINS.values())
    inventories = {**ORIGINS, "evaluation": EVALUATION, "plugins": set(), "indexes": set()}
    for package in packages:
        parts = Path(package["manifest_path"]).relative_to(root).parts
        component_directory = parts[1] if len(parts) == 4 and parts[0] == "components" else None
        if is_concrete_plugin(package["name"], component_directory):
            inventories["plugins"].add(package["name"])
        if component_directory == "indexes" or package["name"].startswith("drasi-index-"):
            inventories["indexes"].add(package["name"])
        if package["name"] in origin_names and not any(
            "lib" in target["kind"] for target in package["targets"]
        ):
            errors.append(f"Analysis origin '{package['name']}' must retain a library target.")

    expected_analysis = {
        "manifest_path": "Cargo.toml",
        "packages": sorted(origin_names),
        "target_kinds": ["library"],
        "incomplete": "deny",
        "proc_macros": True,
        "build_scripts": True,
    }
    if policy.get("version") != 1 or policy.get("analysis") != expected_analysis:
        errors.append("Keep the five library origins and complete, fail-closed analysis in bylaw.toml.")

    expected_selectors = {name: {"packages": sorted(values)} for name, values in inventories.items()}
    for name in ("parsers", "evaluation", "plugins", "indexes"):
        expected_selectors[f"external-{name}"] = {
            "external_crates": sorted({
                target["name"] for package in packages if package["name"] in inventories[name]
                for target in package["targets"] if LIBRARY_KINDS.intersection(target["kind"])
            })
        }
    if policy.get("selectors") != expected_selectors:
        for name in sorted(set(expected_selectors) | set(policy.get("selectors", {}))):
            if policy.get("selectors", {}).get(name) != expected_selectors.get(name):
                errors.append(f"Update selector '{name}' in bylaw.toml to match its workspace inventory.")
    for name, values in inventories.items():
        if not values or not values <= names:
            errors.append(f"Selector '{name}' must resolve to existing workspace packages.")

    rules = policy.get("rule", [])
    if sorted(rule.get("id", "") for rule in rules) != sorted(RULES):
        errors.append("Keep all five distinct architecture rule IDs in bylaw.toml.")
    for rule in rules:
        identifier = rule.get("id")
        if identifier not in RULES:
            continue
        kind, origin, targets = RULES[identifier]
        expected = {
            "id": identifier, "kind": kind, "from": origin, "scope": "both",
            "allowed" if kind == "only-dependencies" else "to": targets,
        }
        if kind == "only-dependencies":
            expected.update(allow_toolchain=True, allow_self=True)
        actual = {key: value for key, value in rule.items() if key != "because"}
        if actual != expected or not rule.get("because", "").strip():
            errors.append(f"Rule '{identifier}' must retain its dependency boundary and reason.")
    return errors


def manifest_violations(policy, metadata):
    selectors = policy["selectors"]
    packages = workspace_packages(metadata)
    declared_names = {dependency["name"] for package in packages for dependency in package["dependencies"]}
    errors = []
    for rule in policy["rule"]:
        origins = set(selectors[rule["from"]]["packages"])
        only = rule["kind"] == "only-dependencies"
        target_selectors = rule["allowed"] if only else rule["to"]
        targets = {
            name for selector in target_selectors
            for name in selectors[selector].get("packages", [])
        }
        if "plugins" in target_selectors:
            targets.update(name for name in declared_names if is_concrete_plugin(name))
        if "indexes" in target_selectors:
            targets.update(name for name in declared_names if name.startswith("drasi-index-"))
        for package in packages:
            if package["name"] not in origins:
                continue
            for dependency in package["dependencies"]:
                if dependency["kind"] is not None:
                    continue
                forbidden = dependency["name"] not in targets if only else dependency["name"] in targets
                if forbidden:
                    errors.append(
                        f"{rule['id']}: {package['name']} -> {dependency['name']} "
                        "(normal dependency, including optional and target-specific declarations)"
                    )
    return errors


def main():
    policy = tomllib.loads(POLICY.read_text())
    result = subprocess.run(
        ["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"],
        cwd=ROOT, text=True, capture_output=True, check=True,
    )
    metadata = json.loads(result.stdout)
    errors = validate_policy(policy, metadata)
    if not errors:
        errors = manifest_violations(policy, metadata)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("Architecture inventories and production manifest boundaries passed.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except subprocess.CalledProcessError as error:
        print(error.stderr, file=sys.stderr)
        sys.exit(error.returncode)
    except (OSError, ValueError, KeyError) as error:
        print(f"Architecture check failed: {error}", file=sys.stderr)
        sys.exit(1)
