#!/usr/bin/env python3

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("build-test-plugins.sh").resolve()
PLUGINS = {
    "drasi-source-mock",
    "drasi-source-http",
    "drasi-reaction-log",
    "drasi-reaction-sse",
    "drasi-reaction-snapshot-test",
    "drasi-identity-test",
    "drasi-bootstrap-scriptfile",
}

FAKE_CARGO = """\
#!/usr/bin/env bash
set -euo pipefail
printf '%s\\n' "$*" >>"$TEST_CALLS"
[[ "$1" == "build" && "$2" == "--lib" && "$3" == "-p" ]]
plugin="$4"
if [[ "$plugin" == "${TEST_FAIL:-}" ]]; then exit 17; fi
if [[ "$plugin" != "drasi-reaction-snapshot-test" ]]; then
    [[ "$5" == "--features" && "$6" == "$plugin/dynamic-plugin" ]]
fi
if [[ "$plugin" != "${TEST_MISSING:-}" ]]; then
    mkdir -p "$CARGO_TARGET_DIR/debug"
    touch "$CARGO_TARGET_DIR/debug/${TEST_PREFIX}${plugin//-/_}.${TEST_EXT}"
fi
"""


class BuildTestPlugins(unittest.TestCase):
    def run_script(self, prefix, ext, **extra_env):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        stale = extra_env.pop("STALE_ARTIFACT", None)
        if stale:
            staged = root / "target/debug/plugins"
            staged.mkdir(parents=True)
            (staged / stale).write_text("stale")
        binary_dir = root / "bin"
        binary_dir.mkdir()
        cargo = binary_dir / "cargo"
        cargo.write_text(FAKE_CARGO)
        cargo.chmod(0o755)
        env = os.environ.copy()
        env.pop("TEST_FAIL", None)
        env.pop("TEST_MISSING", None)
        env.update(
            PATH=f"{binary_dir}{os.pathsep}{env['PATH']}",
            CARGO_TARGET_DIR=str(root / "build"),
            TEST_CALLS=str(root / "calls"),
            TEST_PREFIX=prefix,
            TEST_EXT=ext,
            **extra_env,
        )
        result = subprocess.run(
            ["bash", str(SCRIPT), prefix, ext],
            cwd=root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        return root, result

    def test_builds_and_stages_every_plugin_on_each_platform(self):
        for prefix, ext in (("lib", "so"), ("lib", "dylib"), ("", "dll")):
            with self.subTest(prefix=prefix, ext=ext):
                root, result = self.run_script(prefix, ext)
                self.assertEqual(result.returncode, 0, result.stderr)
                expected = {
                    f"{prefix}{plugin.replace('-', '_')}.{ext}" for plugin in PLUGINS
                }
                staged = root / "target" / "debug" / "plugins"
                self.assertEqual({path.name for path in staged.iterdir()}, expected)
                calls = (root / "calls").read_text().splitlines()
                self.assertEqual(len(calls), len(PLUGINS))
                for plugin in PLUGINS:
                    suffix = (
                        ""
                        if plugin == "drasi-reaction-snapshot-test"
                        else f" --features {plugin}/dynamic-plugin"
                    )
                    self.assertIn(f"build --lib -p {plugin}{suffix}", calls)

    def test_missing_artifact_fails_instead_of_using_stale_output(self):
        root, result = self.run_script(
            "lib",
            "so",
            TEST_MISSING="drasi-source-http",
            STALE_ARTIFACT="libdrasi_source_http.so",
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Expected plugin not found:", result.stderr)
        self.assertIn("libdrasi_source_http.so", result.stderr)
        self.assertEqual(
            (root / "target/debug/plugins/libdrasi_source_http.so").read_text(), "stale"
        )

    def test_cargo_failure_stops_the_build(self):
        root, result = self.run_script("", "dll", TEST_FAIL="drasi-source-http")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(len((root / "calls").read_text().splitlines()), 2)


if __name__ == "__main__":
    unittest.main()
