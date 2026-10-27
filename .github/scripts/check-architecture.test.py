#!/usr/bin/env python3
"""Exercise the checked-in policy and its manifest guards without external services."""

import copy
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest


sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("architecture", ROOT / ".github/scripts/check-architecture.py")
architecture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(architecture)
POLICY = tomllib.loads(architecture.POLICY.read_text())
TOOLCHAIN = (ROOT / "rust-toolchain.toml").read_text()


class PolicyTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        result = subprocess.run(
            ["cargo", "metadata", "--locked", "--offline", "--no-deps", "--format-version", "1"],
            cwd=ROOT, text=True, capture_output=True, check=True,
        )
        cls.metadata = json.loads(result.stdout)

    def test_existing_workspace(self):
        self.assertEqual(architecture.validate_policy(POLICY, self.metadata), [])
        self.assertEqual(architecture.manifest_violations(POLICY, self.metadata), [])

    def test_empty_and_misspelled_selectors(self):
        for names in ([], ["missing-package"]):
            with self.subTest(names=names):
                policy = copy.deepcopy(POLICY)
                policy["selectors"]["ast"]["packages"] = names
                self.assertTrue(architecture.validate_policy(policy, self.metadata))

    def test_weakening_a_rule_is_rejected(self):
        for mutation in ({"scope": "actual"}, {"to": []}, {"severity": "warning"}):
            with self.subTest(mutation=mutation):
                policy = copy.deepcopy(POLICY)
                policy["rule"][1].update(mutation)
                self.assertTrue(architecture.validate_policy(policy, self.metadata))

    def test_incomplete_or_reduced_analysis_is_rejected(self):
        for mutation in ({"incomplete": "allow"}, {"packages": []}, {"build_scripts": False},
                         {"proc_macros": False}, {"all_features": True}):
            with self.subTest(mutation=mutation):
                policy = copy.deepcopy(POLICY)
                policy["analysis"].update(mutation)
                self.assertTrue(architecture.validate_policy(policy, self.metadata))

    def test_analysis_origins_must_keep_their_library_targets(self):
        metadata = copy.deepcopy(self.metadata)
        package = next(package for package in metadata["packages"] if package["name"] == "drasi-query-ast")
        package["targets"] = [{"name": "drasi-query-ast", "kind": ["bin"]}]
        self.assertTrue(architecture.validate_policy(POLICY, metadata))

    def test_new_static_or_dynamic_plugins_and_indexes_need_inventory_updates(self):
        for directory in ("sources", "reactions", "bootstrappers", "identity", "secret_stores", "indexes"):
            for dynamic in (False, True):
                with self.subTest(directory=directory, dynamic=dynamic):
                    metadata = copy.deepcopy(self.metadata)
                    metadata["packages"].append({
                        "id": "new-component", "name": "new-component", "dependencies": [],
                        "features": {"dynamic-plugin": []} if dynamic else {},
                        "targets": [{"name": "new_component", "kind": ["lib"]}],
                        "manifest_path": str(ROOT / "components" / directory / "new-component" / "Cargo.toml"),
                    })
                    metadata["workspace_members"].append("new-component")
                    self.assertTrue(architecture.validate_policy(POLICY, metadata))

    def test_named_plugins_outside_component_directories_need_inventory_updates(self):
        metadata = copy.deepcopy(self.metadata)
        metadata["packages"].append({
            "id": "new-plugin", "name": "drasi-source-new-plugin", "dependencies": [],
            "features": {"dynamic-plugin": []},
            "targets": [{"name": "custom_library_name", "kind": ["lib", "cdylib"]}],
            "manifest_path": str(ROOT / "examples" / "new-plugin" / "Cargo.toml"),
        })
        metadata["workspace_members"].append("new-plugin")
        errors = architecture.validate_policy(POLICY, metadata)
        self.assertTrue(any("'plugins'" in error for error in errors))
        self.assertTrue(any("'external-plugins'" in error for error in errors))

    def test_renamed_optional_target_specific_and_registry_dependencies_are_checked(self):
        cases = [
            ("drasi-ffi-primitives", "serde", "ffi-is-std-only"),
            ("drasi-lib", "drasi-source-application", "runtime-does-not-depend-on-plugins"),
            ("drasi-lib", "drasi-source-future-external", "runtime-does-not-depend-on-plugins"),
            ("drasi-lib", "drasi-index-rocksdb", "runtime-does-not-depend-on-indexes"),
            ("drasi-lib", "drasi-index-future-external", "runtime-does-not-depend-on-indexes"),
            ("drasi-query-ast", "drasi-query-gql", "ast-does-not-depend-on-upper-layers"),
            ("drasi-query-cypher", "drasi-core", "parsers-do-not-depend-on-upper-layers"),
            ("drasi-query-gql", "drasi-lib", "parsers-do-not-depend-on-upper-layers"),
        ]
        for origin, target, rule in cases:
            with self.subTest(origin=origin, target=target):
                metadata = copy.deepcopy(self.metadata)
                package = next(package for package in metadata["packages"] if package["name"] == origin)
                package["dependencies"].append({
                    "name": target, "rename": "convenient_alias", "kind": None,
                    "optional": True, "target": 'cfg(target_os = "windows")',
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                })
                errors = architecture.manifest_violations(POLICY, metadata)
                self.assertEqual(len(errors), 1)
                self.assertIn(rule, errors[0])

    def test_helpers_and_unrelated_external_dependencies_remain_allowed(self):
        metadata = copy.deepcopy(self.metadata)
        package = next(package for package in metadata["packages"] if package["name"] == "drasi-lib")
        for name in ("drasi-source-mapping", "serde"):
            package["dependencies"].append({"name": name, "kind": None})
        self.assertEqual(architecture.manifest_violations(POLICY, metadata), [])

    def test_inactive_renamed_dependency_from_real_cargo_metadata(self):
        names = ["drasi-lib", "drasi-index-rocksdb"]
        with tempfile.TemporaryDirectory(prefix="drasi-manifest-boundary-") as directory:
            root = Path(directory)
            (root / "rust-toolchain.toml").write_text(TOOLCHAIN)
            (root / "Cargo.toml").write_text(
                '[workspace]\nresolver = "2"\nmembers = ' + json.dumps(names) + "\n"
            )
            for name in names:
                package = root / name
                (package / "src").mkdir(parents=True)
                manifest = f'[package]\nname = "{name}"\nversion = "0.0.0"\nedition = "2021"\n'
                if name == "drasi-lib":
                    manifest += (
                        '\n[target.\'cfg(target_os = "windows")\'.dependencies.backend]\n'
                        'package = "drasi-index-rocksdb"\npath = "../drasi-index-rocksdb"\n'
                        'optional = true\n'
                    )
                (package / "Cargo.toml").write_text(manifest)
                (package / "src/lib.rs").write_text("pub struct Marker;\n")
            subprocess.run(
                ["cargo", "generate-lockfile", "--offline"], cwd=root,
                text=True, capture_output=True, check=True,
            )
            result = subprocess.run(
                ["cargo", "metadata", "--locked", "--offline", "--no-deps", "--format-version", "1"],
                cwd=root, text=True, capture_output=True, check=True,
            )
            metadata = json.loads(result.stdout)
            errors = architecture.manifest_violations(POLICY, metadata)
            self.assertEqual(len(errors), 1)
            self.assertIn("runtime-does-not-depend-on-indexes", errors[0])

    def test_runtime_rocksdb_dev_dependency_remains_allowed(self):
        package = next(package for package in self.metadata["packages"] if package["name"] == "drasi-lib")
        self.assertTrue(any(
            dependency["name"] == "drasi-index-rocksdb" and dependency["kind"] == "dev"
            for dependency in package["dependencies"]
        ))
        self.assertEqual(architecture.manifest_violations(POLICY, self.metadata), [])


class BylawFixtureTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        result = subprocess.run(
            ["cargo", "bylaw", "--version"], text=True, capture_output=True, check=True,
        )
        if result.stdout.strip() != "cargo-bylaw 0.1.1":
            raise AssertionError(f"Install the pinned cargo-bylaw 0.1.1 release: {result.stdout}")

    def run_fixture(self, edges, expected_rule=None, unused=False, unresolved=False, external=False, renamed=False):
        names = sorted({
            name for selector in POLICY["selectors"].values()
            for name in selector.get("packages", [])
        })
        with tempfile.TemporaryDirectory(prefix="drasi-architecture-") as directory:
            root = Path(directory)
            (root / "rust-toolchain.toml").write_text(TOOLCHAIN)
            excluded = sorted({target for _, target, _ in edges}) if external else []
            (root / "Cargo.toml").write_text(
                "[workspace]\nresolver = \"2\"\nmembers = "
                + json.dumps([name for name in names if name not in excluded])
                + "\nexclude = " + json.dumps(excluded) + "\n"
            )
            for name in names:
                package = root / name
                (package / "src").mkdir(parents=True)
                manifest = f'[package]\nname = "{name}"\nversion = "0.0.0"\nedition = "2021"\n'
                source = "pub struct Marker;\n"
                for index, (origin, target, dev) in enumerate(edges):
                    if origin != name:
                        continue
                    dependency = f"dependency_{index}" if renamed else target
                    manifest += (
                        f'\n[{"dev-dependencies" if dev else "dependencies"}.{dependency}]\n'
                        f'path = "../{target}"\n'
                    )
                    if renamed:
                        manifest += f'package = "{target}"\n'
                    if not dev and not unused:
                        source += f"pub use {dependency.replace('-', '_')}::Marker as Dependency{index};\n"
                if unresolved and name == "drasi-ffi-primitives":
                    source += "pub fn invalid() { missing_dependency::call(); }\n"
                (package / "Cargo.toml").write_text(manifest)
                (package / "src/lib.rs").write_text(source)
            environment = {**os.environ, "CARGO_NET_OFFLINE": "true"}
            result = subprocess.run(
                ["cargo", "bylaw", "check", "--config", str(architecture.POLICY),
                 "--manifest-path", str(root / "Cargo.toml"), "--format", "json"],
                cwd=root, env=environment, text=True, capture_output=True, check=False,
                timeout=180,
            )
            self.assertTrue(result.stdout, result.stderr)
            report = json.loads(result.stdout)["report"]
            failures = {
                rule["rule_id"] for rule in report["rule_results"] if rule["violations"]
            }
            if unresolved:
                self.assertEqual(result.returncode, 3, result.stdout + result.stderr)
                self.assertTrue(report["analysis_diagnostics"])
                self.assertEqual(failures, set())
            else:
                self.assertEqual(report["analysis_diagnostics"], [], result.stdout)
                self.assertEqual(result.returncode, 1 if expected_rule else 0, result.stdout + result.stderr)
                self.assertEqual(failures, {expected_rule} if expected_rule else set())

    def test_allowed_directions_and_dev_dependency(self):
        self.run_fixture([
            ("drasi-source-application", "drasi-lib", False),
            ("drasi-lib", "drasi-query-cypher", False),
            ("drasi-lib", "drasi-query-gql", False),
            ("drasi-lib", "drasi-query-ast", False),
            ("drasi-lib", "drasi-index-rocksdb", True),
            ("drasi-query-cypher", "drasi-query-ast", False),
            ("drasi-query-gql", "drasi-query-ast", False),
        ])

    def test_each_boundary_rejects_forbidden_dependencies(self):
        cases = [
            ("drasi-ffi-primitives", "drasi-query-ast", "ffi-is-std-only"),
            ("drasi-lib", "drasi-source-application", "runtime-does-not-depend-on-plugins"),
            ("drasi-lib", "drasi-index-rocksdb", "runtime-does-not-depend-on-indexes"),
            ("drasi-query-ast", "drasi-query-cypher", "ast-does-not-depend-on-upper-layers"),
            ("drasi-query-cypher", "drasi-core", "parsers-do-not-depend-on-upper-layers"),
            ("drasi-query-gql", "drasi-lib", "parsers-do-not-depend-on-upper-layers"),
        ]
        for origin, target, rule in cases:
            with self.subTest(origin=origin, target=target):
                self.run_fixture([(origin, target, False)], rule)

    def test_unused_manifest_dependency_is_rejected(self):
        self.run_fixture(
            [("drasi-lib", "drasi-index-rocksdb", False)],
            "runtime-does-not-depend-on-indexes", unused=True,
        )

    def test_renamed_external_plugin_is_rejected(self):
        self.run_fixture(
            [("drasi-lib", "drasi-source-application", False)],
            "runtime-does-not-depend-on-plugins", external=True, renamed=True,
        )

    def test_analysis_errors_are_not_architecture_violations(self):
        self.run_fixture([], unresolved=True)


if __name__ == "__main__":
    unittest.main()
