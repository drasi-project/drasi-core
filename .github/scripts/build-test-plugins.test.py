#!/usr/bin/env python3

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("build-test-plugins.sh").resolve()
NATIVE = "drasi-computation-source-counter"
LEGACY = {
    "drasi-source-mock", "drasi-source-http", "drasi-reaction-http",
    "drasi-reaction-log", "drasi-reaction-sse", "drasi-reaction-snapshot-test",
    "drasi-identity-test", "drasi-bootstrap-scriptfile", "drasi-secret-store-file",
}
FIXTURES = {
    "native_recovery", "native_consumer_progress_metadata",
    "native_bootstrap_coordination_metadata", "native_bootstrap_coordination_partial",
    "native_activation_credentials", "native_custom_query", "native_scheduling",
    "instance_host_fixture",
}
FAKE_CARGO = r"""#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
root = pathlib.Path(os.environ["TEST_ROOT"])
with (root / "calls").open("a") as out:
    out.write(json.dumps(args) + "\n")
assert "--locked" in args
if args[0] == "metadata":
    print(json.dumps({
        "target_directory": str(root / "custom target"),
        "packages": [] if os.environ.get("TEST_NO_NATIVE") else [{
            "name": "drasi-computation-source-counter",
            "metadata": {"drasi-plugin": {"abi-family": "computation"}},
        }],
    }))
    sys.exit(0)
assert args[0] == "build"
package = args[args.index("-p") + 1]
if package == os.environ.get("TEST_FAIL"):
    sys.exit(17)
prefix, ext = os.environ["TEST_PREFIX"], os.environ["TEST_EXT"]
# Later feature configurations can replace ordinary Cargo outputs.
for previous in (root / "custom target/debug").glob(f"*{ext}"):
    previous.write_text("rebuilt without plugin exports")
def artifact(name, example=False):
    if name == os.environ.get("TEST_MISSING"):
        return
    directory = root / "custom target/debug"
    if example:
        directory /= "examples"
    directory.mkdir(parents=True, exist_ok=True)
    (directory / f"{prefix}{name}.{ext}").write_text("fresh")
if "--lib" in args:
    if package != "drasi-reaction-snapshot-test":
        assert args[args.index("--features") + 1] == package + "/dynamic-plugin"
    artifact(package.replace("-", "_"))
elif package == "drasi-host-sdk":
    assert "--examples" in args
    for name in ("native_recovery", "native_consumer_progress_metadata",
                 "native_bootstrap_coordination_metadata", "native_bootstrap_coordination_partial",
                 "native_activation_credentials", "native_custom_query", "native_scheduling"):
        artifact(name, True)
else:
    assert package == "drasi-instance-host"
    assert args[args.index("--features") + 1] == package + "/dynamic-plugin"
    artifact("instance_host_fixture", True)
"""


class BuildTestPlugins(unittest.TestCase):
    def run_script(self, prefix="lib", ext="so", **extra):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        binary = root / "bin"
        binary.mkdir()
        cargo = binary / "cargo"
        cargo.write_text(FAKE_CARGO)
        cargo.chmod(0o755)
        staged = root / "custom target/debug/plugins"
        staged.mkdir(parents=True)
        (staged / f"{prefix}drasi_computation_standard.{ext}").write_text("obsolete")
        (staged / f"{prefix}drasi_source_mock.{ext}").write_text("stale")
        env = os.environ.copy()
        for key in ("TEST_FAIL", "TEST_MISSING", "TEST_NO_NATIVE"):
            env.pop(key, None)
        env.update(
            PATH=f"{binary}{os.pathsep}{env['PATH']}",
            TEST_ROOT=str(root), TEST_PREFIX=prefix, TEST_EXT=ext, **extra,
        )
        result = subprocess.run(
            ["bash", str(SCRIPT), prefix, ext], cwd=root,
            env=env, text=True, capture_output=True,
        )
        return root, result

    def test_builds_and_stages_plugins_and_fixtures_on_each_platform(self):
        for prefix, ext in (("lib", "so"), ("lib", "dylib"), ("", "dll")):
            with self.subTest(ext=ext):
                root, result = self.run_script(prefix, ext)
                self.assertEqual(result.returncode, 0, result.stderr)
                staged = root / "custom target/debug/plugins"
                expected = {
                    f"{prefix}{name.replace('-', '_')}.{ext}"
                    for name in LEGACY | {NATIVE}
                }
                self.assertEqual({p.name for p in staged.iterdir()}, expected | {"examples"})
                self.assertEqual(
                    {p.name for p in (staged / "examples").iterdir()},
                    {f"{prefix}{name}.{ext}" for name in FIXTURES},
                )
                self.assertTrue(all(p.read_text() == "fresh" for p in staged.glob(f"*.{ext}")))

    def test_missing_artifact_fails_instead_of_accepting_stale_staged_file(self):
        _, result = self.run_script(TEST_MISSING="drasi_source_mock")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Expected plugin not found or empty:", result.stderr)

    def test_missing_fixture_fails(self):
        _, result = self.run_script(TEST_MISSING="native_recovery")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("native_recovery", result.stderr)

    def test_cargo_failure_stops_the_build(self):
        root, result = self.run_script(TEST_FAIL="drasi-source-http")
        self.assertEqual(result.returncode, 17)
        calls = [json.loads(line) for line in (root / "calls").read_text().splitlines()]
        self.assertEqual(len(calls), 3)

    def test_empty_native_inventory_fails(self):
        _, result = self.run_script(TEST_NO_NATIVE="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No native plugins found", result.stderr)


if __name__ == "__main__":
    unittest.main()
