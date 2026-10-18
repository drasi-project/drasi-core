#!/usr/bin/env python3
"""Candidate, real Cargo packaging, and workflow contract tests; no registry writes."""

import copy
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("candidate", Path(__file__).with_name("release-candidate.py"))
candidate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(candidate)
REPAIR_SPEC = importlib.util.spec_from_file_location(
    "repair_tests", Path(__file__).with_name("repair-plugin-signatures.test.py"))
repair_tests = importlib.util.module_from_spec(REPAIR_SPEC)
REPAIR_SPEC.loader.exec_module(repair_tests)


class CandidateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.root = self.base / "source"
        self.root.mkdir()
        (self.root / "Cargo.toml").write_text(
            '[workspace]\nmembers=["base","plugin","internal","excluded"]\nresolver="2"\n')
        for directory, name, extra in (
            ("base", "candidate-fixture-base", ""),
            ("plugin", "drasi-source-fixture", (
                '[features]\ndynamic-plugin=[]\n'
                '[dependencies]\ncandidate-fixture-base={path="../base",version="99.0.0"}\n')),
            ("internal", "candidate-internal", ""),
            ("excluded", "candidate-excluded", ""),
        ):
            folder = self.root / directory
            (folder / "src").mkdir(parents=True)
            (folder / "src/lib.rs").write_text("pub fn value() -> bool { true }\n")
            (folder / "Cargo.toml").write_text(
                f'[package]\nname="{name}"\nversion="99.0.0"\nedition="2021"\n'
                'license="Apache-2.0"\ndescription="Packaging fixture"\n'
                + ('publish=false\n' if directory == "internal" else "") + extra)
        (self.root / "rust-toolchain.toml").write_text((ROOT / "rust-toolchain.toml").read_text())
        (self.root / "release-plz.toml").write_text(
            '[workspace]\ndependencies_update=false\n'
            '[[package]]\nname="candidate-excluded"\nrelease=false\npublish=false\n')
        self.command("cargo", "generate-lockfile", "--offline")
        self.command("git", "init", "-q")
        self.command("git", "add", ".")
        self.command("git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                     "commit", "-qm", "fixture")
        self.bundle = self.base / "bundle"
        with patch.dict(os.environ, {"GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "2",
                                     "GITHUB_REF": "refs/heads/main",
                                     "ALLOW_BRANCH_TEST": "false",
                                     "GITHUB_REPOSITORY": "drasi-project/drasi-core"}):
            candidate.freeze(self.root, self.bundle)
        self.expected = candidate.sha(self.bundle / "candidate.json")
        self.prepared = self.base / "prepared"
        self.data = candidate.restore(self.bundle, self.expected, self.prepared)

    def command(self, *args):
        result = subprocess.run(args, cwd=self.root, capture_output=True, text=True)
        self.assertEqual(0, result.returncode, f"{args}: {result.stderr}")
        return result

    def test_restore_and_exclusions(self):
        self.assertEqual({"candidate-fixture-base", "drasi-source-fixture"},
                         {p["name"] for p in self.data["crates"]})
        self.assertEqual(["drasi-source-fixture"], [p["name"] for p in self.data["plugins"]])
        self.assertEqual("locked", self.data["mode"])
        self.assertEqual(self.data["baseline_lock_sha256"], self.data["lock_sha256"])
        self.assertEqual(candidate.sha(self.root / "Cargo.lock"), candidate.sha(self.prepared / "Cargo.lock"))

    def test_new_prepared_changelog_is_preserved_without_unrelated_files(self):
        (self.root / "plugin/CHANGELOG.md").write_text("# First release\n")
        (self.root / "unrelated-secret.txt").write_text("must-not-be-bundled")
        bundle = self.base / "with-changelog"
        candidate.freeze(self.root, bundle)
        restored = self.base / "with-changelog-source"
        candidate.restore(bundle, candidate.sha(bundle / "candidate.json"), restored)
        self.assertEqual("# First release\n", (restored / "plugin/CHANGELOG.md").read_text())
        self.assertFalse((restored / "unrelated-secret.txt").exists())

    def test_real_multi_package_verification_of_unpublished_versions(self):
        report = self.base / "packages.json"
        nested = self.root / "prepared"
        candidate.restore(self.bundle, self.expected, nested)
        candidate.package(self.bundle, self.expected, nested, report)
        self.assertEqual(2, len(json.loads(report.read_text())["archives"]))
        self.assertEqual(candidate.sha(self.root / "Cargo.lock"), candidate.sha(self.prepared / "Cargo.lock"))

    def test_packaged_source_failure_is_not_skipped(self):
        # This crate builds in the workspace, but Cargo's package excludes a required file.
        path = self.root / "plugin/Cargo.toml"
        path.write_text(path.read_text().replace('edition="2021"', 'edition="2021"\nexclude=["src/needed.rs"]'))
        (self.root / "plugin/src/lib.rs").write_text('include!("needed.rs");\n')
        (self.root / "plugin/src/needed.rs").write_text("pub fn needed() {}\n")
        self.command("git", "add", ".")
        self.command("cargo", "check", "--locked", "--offline", "-p", "drasi-source-fixture")
        broken = self.base / "broken"
        candidate.freeze(self.root, broken)
        digest = candidate.sha(broken / "candidate.json")
        extracted = self.base / "broken-source"
        candidate.restore(broken, digest, extracted)
        with self.assertRaises(subprocess.CalledProcessError):
            candidate.package(broken, digest, extracted, self.base / "must-not-exist.json")
        self.assertFalse((self.base / "must-not-exist.json").exists())

    def test_missing_and_changed_lock_fail(self):
        lock = self.prepared / "Cargo.lock"
        lock.write_text(lock.read_text() + "\n")
        with self.assertRaisesRegex(ValueError, "lock hash"):
            candidate.check(self.bundle, self.expected, self.prepared)
        lock.unlink()
        with self.assertRaisesRegex(ValueError, "Missing restored"):
            candidate.check(self.bundle, self.expected, self.prepared)

    def test_changed_manifest_fails(self):
        (self.prepared / "plugin/Cargo.toml").write_text("[package]\n")
        with self.assertRaisesRegex(ValueError, "source mismatch"):
            candidate.check(self.bundle, self.expected, self.prepared)

    def test_bundle_tampering_fails(self):
        with self.assertRaisesRegex(ValueError, "manifest hash"):
            candidate.load(self.bundle, "0" * 64)
        with (self.bundle / "source.tar").open("ab") as stream:
            stream.write(b"corruption")
        with self.assertRaisesRegex(ValueError, "source hash"):
            candidate.load(self.bundle, self.expected)

    def test_manifest_lock_mismatch_fails_cargo_without_resolution(self):
        path = self.prepared / "base/Cargo.toml"
        path.write_text(path.read_text().replace('version="99.0.0"', 'version="99.0.1"'))
        lock_hash = candidate.sha(self.prepared / "Cargo.lock")
        with self.assertRaises(subprocess.CalledProcessError):
            candidate.metadata(self.prepared)
        self.assertEqual(lock_hash, candidate.sha(self.prepared / "Cargo.lock"))

    def test_namespace_qualifies_catalog_and_plugins(self):
        self.assertEqual(
            ["drasi-core-nightly/drasi-plugin-directory", "drasi-core-nightly/source/fixture"],
            candidate.registry_packages(candidate.TEST_REGISTRY, self.data))
        self.assertEqual(["drasi-plugin-directory", "source/fixture"],
                         candidate.registry_packages("ghcr.io/drasi-project", self.data))
        with self.assertRaises(ValueError):
            candidate.registry_packages("ghcr.io/user:secret@host")

    def test_workspace_update_preserves_external_lock_baseline(self):
        manifest = self.root / "base/Cargo.toml"
        manifest.write_text(manifest.read_text() + '[dependencies]\nitoa="1"\n')
        self.command("cargo", "generate-lockfile")
        self.command("cargo", "update", "-p", "itoa", "--precise", "1.0.17")
        before = candidate.metadata(self.root)
        manifest.write_text(manifest.read_text().replace('version="99.0.0"', 'version="99.0.1"'))
        self.command("cargo", "update", "--workspace")
        after = candidate.metadata(self.root)
        external = lambda data: {
            p["name"]: p["version"] for p in data["packages"] if p.get("source")
        }
        self.assertEqual({"itoa": "1.0.17"}, external(before))
        self.assertEqual(external(before), external(after))

    def test_normal_release_config_does_not_refresh_dependencies(self):
        config = candidate.tomllib.loads((ROOT / "release-plz.toml").read_text())
        self.assertIs(config["workspace"]["dependencies_update"], False)

    def handoff_fixture(self):
        self.data["versions"].update({
            "drasi-plugin-sdk": "1.0.0", "drasi-core": "1.0.0", "drasi-lib": "1.0.0"})
        candidate.write_json(self.bundle / "candidate.json", self.data)
        self.expected = candidate.sha(self.bundle / "candidate.json")
        reports = self.base / "reports"
        reports.mkdir()
        for target, suffix in candidate.TARGETS.items():
            reference = f"{candidate.TEST_REGISTRY}/source/fixture"
            artifact = self.data["plugins"][0] | {
                "target": target, "reference": f"{reference}:nightly-123-2-{suffix}",
                "digest_reference": f"{reference}@sha256:" + "a" * 64,
                "sdk_version": "1.0.0", "core_version": "1.0.0", "lib_version": "1.0.0",
                "signed": True, "verified": True,
            }
            candidate.write_json(reports / f"{target}.json", {
                "schema_version": 1,
                "target": target, "candidate_sha256": self.expected,
                "lock_sha256": self.data["lock_sha256"], "source_sha256": self.data["source_sha256"],
                "artifact_failures": 0, "catalog_failures": 0, "catalog_successes": 1,
                "artifacts": [artifact],
            })
        packages = self.base / "packages.json"
        candidate.write_json(packages, {"candidate_sha256": self.expected, "archives": [
            {"name": f"{p['name']}-{p['version']}.crate", "sha256": "b" * 64}
            for p in self.data["crates"]]})
        return reports, packages

    def produce(self, reports, packages):
        with patch.dict(os.environ, {"GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "2"}):
            candidate.handoff(self.bundle, self.expected, reports, packages, self.base / "ready.json")

    def test_complete_handoff(self):
        self.produce(*self.handoff_fixture())
        result = json.loads((self.base / "ready.json").read_text())
        self.assertEqual(8, len(result["artifacts"]))
        self.assertEqual("locked", result["mode"])
        self.assertEqual(candidate.IDENTITY, result["certificate_identity"])
        self.assertEqual("nightly-candidate-123-2", result["candidate_artifact"])

    def test_branch_handoff_uses_exact_branch_identity(self):
        self.data.update(source_ref="refs/heads/test/release", branch_test=True)
        self.produce(*self.handoff_fixture())
        result = json.loads((self.base / "ready.json").read_text())
        self.assertTrue(result["branch_test"])
        self.assertEqual("refs/heads/test/release", result["source_ref"])
        self.assertEqual(candidate.IDENTITY.replace("refs/heads/main", "refs/heads/test/release"),
                         result["certificate_identity"])

    def test_freeze_records_explicit_branch_test_context(self):
        destination = self.base / "branch-candidate"
        with patch.dict(os.environ, {
            "GITHUB_REF": "refs/heads/test/release", "ALLOW_BRANCH_TEST": "true",
        }):
            candidate.freeze(self.root, destination)
        result = json.loads((destination / "candidate.json").read_text())
        self.assertTrue(result["branch_test"])
        self.assertEqual("refs/heads/test/release", result["source_ref"])
        self.assertEqual("locked", result["mode"])

    def test_branch_handoff_without_opt_in_fails(self):
        self.data.update(source_ref="refs/heads/test/release", branch_test=False)
        with self.assertRaisesRegex(ValueError, "explicit branch-test"):
            self.produce(*self.handoff_fixture())
        self.assertFalse((self.base / "ready.json").exists())

    def test_handoff_rejects_failed_stale_unsigned_or_mutable_outputs(self):
        reports, packages = self.handoff_fixture()
        path = next(reports.glob("*.json"))
        original = json.loads(path.read_text())
        for field, value in (("artifact_failures", 1), ("catalog_failures", 1),
                             ("catalog_successes", 0), ("lock_sha256", "bad"),
                             ("candidate_sha256", "bad"), ("artifacts", [])):
            with self.subTest(field=field):
                candidate.write_json(path, original | {field: value})
                with self.assertRaises(ValueError):
                    self.produce(reports, packages)
        for field, value in (("signed", False), ("verified", False),
                             ("digest_reference", "test:latest"), ("version", "0.0.0"),
                             ("reference", "ghcr.io/drasi-project/source/fixture:official"),
                             ("sdk_version", "0.0.0")):
            altered = copy.deepcopy(original)
            altered["artifacts"][0][field] = value
            candidate.write_json(path, altered)
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.produce(reports, packages)
        path.unlink()
        with self.assertRaisesRegex(ValueError, "target inventory"):
            self.produce(reports, packages)
        self.assertFalse((self.base / "ready.json").exists())

    def test_handoff_rejects_wrong_run_attempt(self):
        reports, packages = self.handoff_fixture()
        with patch.dict(os.environ, {"GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "3"}):
            with self.assertRaisesRegex(ValueError, "run and attempt"):
                candidate.handoff(self.bundle, self.expected, reports, packages, self.base / "ready.json")


class WorkflowTests(unittest.TestCase):
    def test_freeze_rejects_removed_dependency_refresh_mode(self):
        result = subprocess.run(
            [sys.executable, str(ROOT / ".github/scripts/release-candidate.py"),
             "freeze", "--mode", "fresh"],
            capture_output=True, text=True)
        self.assertEqual(2, result.returncode)
        self.assertIn("unrecognized arguments: --mode fresh", result.stderr)

    def test_publication_context_limits_branch_opt_in_to_isolated_nightly(self):
        env = {
            "GITHUB_REPOSITORY": "drasi-project/drasi-core",
            "GITHUB_REF": "refs/heads/test/release",
            "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_WORKFLOW_REF":
                "drasi-project/drasi-core/.github/workflows/nightly.yml@refs/heads/test/release",
            "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "2",
        }
        inputs = {
            "allow_branch_test": True,
            "registry": candidate.TEST_REGISTRY, "tag": "nightly-123-2",
            "candidate_artifact": "nightly-candidate-123-2", "candidate_sha256": "a" * 64,
            "sign": True, "dry_run": False, "skip_visibility": False,
        }
        self.assertEqual(candidate.IDENTITY.replace("refs/heads/main", env["GITHUB_REF"]),
                         candidate.publication_context(inputs, env))
        for key, value in (
            ("allow_branch_test", False), ("allow_branch_test", "true"),
            ("registry", "ghcr.io/drasi-project"), ("tag", "latest"),
            ("candidate_artifact", "nightly-candidate-123-1"), ("candidate_sha256", ""),
            ("sign", False), ("dry_run", True), ("skip_visibility", True),
            ("ref", "other-branch"), ("pre_release", "dev"),
            ("repair_batch", "release-2026-09-25-darwin-arm64"), ("repair_action", "execute"),
        ):
            with self.subTest(input=key), self.assertRaises(ValueError):
                candidate.publication_context(inputs | {key: value}, env)
        for key, value in (
            ("GITHUB_REPOSITORY", "someone/drasi-core"),
            ("GITHUB_REF", "refs/tags/v1"), ("GITHUB_EVENT_NAME", "schedule"),
            ("GITHUB_EVENT_NAME", "pull_request"),
            ("GITHUB_WORKFLOW_REF",
             "drasi-project/drasi-core/.github/workflows/publish-plugins.yml@refs/heads/test/release"),
            ("GITHUB_WORKFLOW_REF",
             "drasi-project/drasi-core/.github/workflows/nightly.yml@refs/heads/main"),
        ):
            with self.subTest(context=key), self.assertRaises(ValueError):
                candidate.publication_context(inputs, env | {key: value})
        self.assertEqual(candidate.IDENTITY,
                         candidate.publication_context({}, env | {"GITHUB_REF": "refs/heads/main"}))
        # Exercise the same CLI gate used before publication/repair selection.
        for allowed in (True, False):
            result = subprocess.run(
                [sys.executable, str(ROOT / ".github/scripts/release-candidate.py"),
                 "publication-context"],
                env=os.environ | env | {
                    "WORKFLOW_INPUTS": json.dumps(inputs | {"allow_branch_test": allowed}),
                    "GITHUB_OUTPUT": os.devnull,
                }, capture_output=True, text=True)
            self.assertEqual(0 if allowed else 1, result.returncode, result.stderr)

    def test_make_publish_all_does_not_hide_mixed_publication_failures(self):
        with tempfile.TemporaryDirectory() as directory:
            cargo = Path(directory) / "cargo"
            cargo.write_text(
                '#!/bin/sh\n'
                'case "$*" in *publish-plugins*--target\\ fail*) exit 1;; esac\n'
                'exit 0\n')
            cargo.chmod(0o755)
            result = subprocess.run(
                ["make", "publish-all", "PUBLISH_TARGETS=fail:first ok:second"],
                cwd=ROOT, env=os.environ | {"PATH": directory + os.pathsep + os.environ["PATH"]},
                capture_output=True, text=True)
            self.assertNotEqual(0, result.returncode)
            self.assertIn("1 succeeded, 1 failed", result.stdout)

    def test_nightly_is_locked_signed_and_isolated(self):
        document = repair_tests.workflow(ROOT / ".github/workflows/nightly.yml")
        inputs = document["on"]["workflow_dispatch"]["inputs"]
        self.assertEqual({"allow_branch_test"}, set(inputs))
        self.assertFalse(inputs["allow_branch_test"]["default"])
        jobs = document["jobs"]
        prepare_script = "\n".join(s.get("run", "") for s in jobs["prepare"]["steps"])
        self.assertIn("release-plz update", prepare_script)
        self.assertNotIn("cargo update", prepare_script)
        publish = jobs["publish-plugins"]
        self.assertEqual(candidate.TEST_REGISTRY, publish["with"]["registry"])
        self.assertTrue(publish["with"]["sign"])
        self.assertFalse(publish["with"]["skip_visibility"])
        self.assertEqual({"prepare", "package"}, set(publish["needs"]))
        self.assertEqual({"prepare", "package", "publish-plugins"}, set(jobs["handoff"]["needs"]))
        self.assertNotIn("CARGO_REGISTRY_TOKEN", json.dumps(document))
        self.assertIn("refs/heads/main", jobs["prepare"]["if"])
        self.assertIn("github.event_name == 'workflow_dispatch'", jobs["prepare"]["if"])
        self.assertEqual("${{ inputs.allow_branch_test == true }}",
                         publish["with"]["allow_branch_test"])

    def test_supported_callers_select_publication_with_inherited_inputs(self):
        publication = repair_tests.workflow(ROOT / ".github/workflows/publish-plugins.yml")
        definitions = publication["on"]["workflow_call"]["inputs"]
        defaults = {name: definition["default"] for name, definition in definitions.items()}
        callers = {
            filename: repair_tests.workflow(ROOT / ".github/workflows" / filename)
            for filename in ("nightly.yml", "release-plz.yml")
        }
        selection = publication["jobs"]["select-mode"]["steps"][1]["run"]
        for filename, event, branch, overrides in (
            ("nightly.yml", "schedule", "main", {}),
            ("nightly.yml", "workflow_dispatch", "main", {}),
            ("nightly.yml", "workflow_dispatch", "ruokun-niu-core-release-safeguards",
             {"allow_branch_test": True}),
            ("release-plz.yml", "push", "main", {}),
            ("release-plz.yml", "workflow_dispatch", "main", {}),
            ("release-plz.yml", "workflow_dispatch", "main", {"force_publish": True}),
        ):
            with self.subTest(caller=filename, event=event, branch=branch, overrides=overrides), \
                    tempfile.TemporaryDirectory() as directory:
                caller = callers[filename]
                dispatch_inputs = {
                    name: definition["default"]
                    for name, definition in caller["on"]["workflow_dispatch"]["inputs"].items()
                } if event == "workflow_dispatch" else {}
                dispatch_inputs.update(overrides)
                context = {
                    "inputs": dispatch_inputs,
                    "github": {"run_id": "38933476222", "run_attempt": "1"},
                    "needs": {
                        job: {"outputs": {"candidate_sha256": "a" * 64}}
                        for job in ("prepare", "release-plz")
                    },
                }

                def substitute(match):
                    value = context
                    for key in match[1].strip().split("."):
                        value = value[key]
                    return str(value)

                # GitHub can include caller-only dispatch inputs in toJSON(inputs).
                inputs = defaults | dispatch_inputs
                for name, value in caller["jobs"]["publish-plugins"]["with"].items():
                    if isinstance(value, str):
                        value = (repair_tests.expression(value, context)
                                 if definitions[name]["type"] == "boolean"
                                 else re.sub(r"\$\{\{(.*?)\}\}", substitute, value))
                    inputs[name] = value
                ref = f"refs/heads/{branch}"
                outputs = Path(directory) / "outputs"
                result = subprocess.run(
                    ["bash", "-e", "-c", selection], cwd=ROOT, capture_output=True, text=True,
                    env=os.environ | {
                        "WORKFLOW_INPUTS": json.dumps(inputs),
                        "GITHUB_REPOSITORY": "drasi-project/drasi-core",
                        "GITHUB_REF": ref,
                        "GITHUB_EVENT_NAME": event,
                        "GITHUB_WORKFLOW_REF":
                            f"drasi-project/drasi-core/.github/workflows/{filename}@{ref}",
                        "GITHUB_RUN_ID": "38933476222", "GITHUB_RUN_ATTEMPT": "1",
                        "GITHUB_OUTPUT": str(outputs),
                    })
                self.assertEqual(0, result.returncode, result.stdout + result.stderr)
                self.assertEqual({
                    "certificate_identity": candidate.IDENTITY.replace("refs/heads/main", ref),
                    "mode": "publish",
                }, dict(line.split("=", 1) for line in outputs.read_text().splitlines()))

    def test_release_and_nightly_share_candidate_and_publication_safeguards(self):
        publication = repair_tests.workflow(ROOT / ".github/workflows/publish-plugins.yml")
        defaults = {
            name: definition["default"]
            for name, definition in publication["on"]["workflow_call"]["inputs"].items()
        }
        for filename, prepare_name, prefix, registry in (
            ("nightly.yml", "prepare", "nightly", candidate.TEST_REGISTRY),
            ("release-plz.yml", "release-plz", "release", "ghcr.io/drasi-project"),
        ):
            with self.subTest(caller=filename):
                jobs = repair_tests.workflow(ROOT / ".github/workflows" / filename)["jobs"]
                prepare = jobs[prepare_name]
                steps = prepare["steps"]
                self.assertIn("./.github/actions/install-release-tools",
                              [step.get("uses") for step in steps])
                freeze = next(step for step in steps if step.get("id") == "freeze")
                self.assertIn("python3 .github/scripts/release-candidate.py freeze", freeze["run"])
                self.assertEqual("${{ steps.freeze.outputs.candidate_sha256 }}",
                                 prepare["outputs"]["candidate_sha256"])
                self.assertNotIn("cargo update", "\n".join(step.get("run", "") for step in steps))

                publish = jobs["publish-plugins"]
                self.assertEqual("./.github/workflows/publish-plugins.yml", publish["uses"])
                effective = defaults | publish["with"]
                self.assertIs(effective["sign"], True)
                self.assertIs(effective["dry_run"], False)
                self.assertIs(effective["skip_visibility"], False)
                self.assertEqual(registry, effective["registry"])
                self.assertEqual("", effective["ref"])
                self.assertEqual("", effective["pre_release"])
                tag = "nightly-${{ github.run_id }}-${{ github.run_attempt }}" if prefix == "nightly" else ""
                self.assertEqual(tag, effective["tag"])
                artifact = f"{prefix}-candidate-${{{{ github.run_id }}}}-${{{{ github.run_attempt }}}}"
                self.assertEqual(artifact, effective["candidate_artifact"])
                self.assertTrue(any(
                    step.get("uses", "").startswith("actions/upload-artifact@")
                    and step["with"]["name"] == artifact for step in steps))
                self.assertEqual(f"${{{{ needs.{prepare_name}.outputs.candidate_sha256 }}}}",
                                 effective["candidate_sha256"])
                self.assertEqual({"contents": "read", "packages": "write", "id-token": "write"},
                                 publish["permissions"])
                self.assertEqual({"PACKAGES_ADMIN_TOKEN": "${{ secrets.PACKAGES_ADMIN_TOKEN }}"},
                                 publish["secrets"])

    def test_release_preparation_and_dry_run_gates_remain_distinct_from_nightly(self):
        jobs = repair_tests.workflow(ROOT / ".github/workflows/release-plz.yml")["jobs"]
        steps = jobs["release-plz"]["steps"]
        release_pr = next(step for step in steps if "release-plz release-pr " in step.get("run", ""))
        freeze = next(step for step in steps if step.get("id") == "freeze")
        publish_crates = next(step for step in steps if step.get("name") == "Publish crates")
        published = next(step for step in steps if step.get("id") == "publish_result")
        self.assertIn("release-plz release ", publish_crates["run"])
        self.assertIn("release-candidate.py check", published["run"])
        self.assertLess(steps.index(freeze), steps.index(publish_crates))
        self.assertLess(steps.index(publish_crates), steps.index(published))
        for release_commit, force, dry_run in (
            (False, False, False), (True, False, False), (False, True, False),
            (True, True, False), (True, False, True), (False, True, True),
        ):
            with self.subTest(release_commit=release_commit, force=force, dry_run=dry_run):
                context = {
                    "inputs": {"force_publish": force, "dry_run": dry_run},
                    "steps": {"check_release": {"outputs": {
                        "is_release": str(release_commit).lower(),
                    }}},
                }
                self.assertEqual(not (release_commit or force or dry_run),
                                 repair_tests.expression(release_pr["if"], context))
                for step in (freeze, publish_crates, published):
                    self.assertEqual((release_commit or force) and not dry_run,
                                     repair_tests.expression(step["if"], context))
        self.assertEqual("release-plz", jobs["publish-plugins"]["needs"])
        for result in ("true", "false", ""):
            self.assertEqual(result == "true", repair_tests.expression(
                jobs["publish-plugins"]["if"],
                {"needs": {"release-plz": {"outputs": {"published": result}}}}))

        nightly = repair_tests.workflow(ROOT / ".github/workflows/nightly.yml")["jobs"]
        prepare_steps = nightly["prepare"]["steps"]
        update = next(step for step in prepare_steps if step.get("run") == "release-plz update")
        freeze = next(step for step in prepare_steps if step.get("id") == "freeze")
        self.assertLess(prepare_steps.index(update), prepare_steps.index(freeze))
        self.assertEqual("prepare", nightly["package"]["needs"])
        self.assertTrue(any("release-candidate.py package " in step.get("run", "")
                            for step in nightly["package"]["steps"]))
        self.assertNotIn("release-plz release", json.dumps(nightly))
        self.assertEqual({"prepare", "package"}, set(nightly["publish-plugins"]["needs"]))
        self.assertEqual({"prepare", "package", "publish-plugins"}, set(nightly["handoff"]["needs"]))

    def test_all_targets_consume_same_candidate_and_keep_native_checks(self):
        document = repair_tests.workflow(ROOT / ".github/workflows/publish-plugins.yml")
        job = document["jobs"]["build-and-publish"]
        matrix = job["strategy"]["matrix"]["include"]
        self.assertEqual(candidate.TARGETS, {p["target"]: p["arch_suffix"] for p in matrix})
        for platform in matrix:
            if platform["target"].endswith("linux-gnu"):
                self.assertEqual("2.28", platform["glibc"])
                self.assertIn("@sha256:", platform["manylinux_image"])
        self.assertIn("prepare-candidate", job["needs"])
        self.assertEqual("${{ needs.prepare-candidate.outputs.sha256 }}",
                         job["env"]["CANDIDATE_SHA256"])
        script = "\n".join(s.get("run", "") for s in job["steps"])
        self.assertIn("restore --bundle candidate", script)
        self.assertIn("check --bundle candidate", script)
        self.assertNotIn("cargo run -p xtask", script)
        self.assertNotIn("cargo update", script)
        publish = next(s for s in job["steps"] if s.get("name") == "Publish plugins (per-architecture tag)")
        self.assertEqual("", publish["env"]["COSIGN_REPOSITORY"])
        self.assertEqual("${{ needs.select-mode.outputs.certificate_identity }}",
                         publish["env"]["COSIGN_VERIFY_CERTIFICATE_IDENTITY"])


if __name__ == "__main__":
    unittest.main()
