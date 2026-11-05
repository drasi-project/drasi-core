#!/usr/bin/env python3

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("install-protoc.sh").resolve()
CHECKSUMS = {
    "x86_64": "3e866620c5be27664f3d2fa2d656b5f3e09b5152b42f1bedbf427b333e90021a",
    "aarch_64": "6427349140e01f06e049e707a58709a4f221ae73ab9a0425bc4a00c8d0e1ab32",
}

FAKE_COMMAND = """\
#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

command = Path(sys.argv[0]).name
stdin = sys.stdin.read() if command == "sha256sum" else ""
with open(os.environ["TEST_CALLS"], "a") as calls:
    calls.write(json.dumps([command, sys.argv[1:], stdin]) + "\\n")
if command == os.environ.get("TEST_FAIL"):
    print(f"{command} failed", file=sys.stderr)
    sys.exit(17)
if command == "uname":
    print(os.environ["TEST_HOST_ARCH"])
elif command == "curl":
    Path(sys.argv[sys.argv.index("--output") + 1]).write_bytes(b"archive")
elif command == "unzip":
    prefix = Path(sys.argv[sys.argv.index("-d") + 1])
    (prefix / "bin").mkdir(parents=True)
    (prefix / "include/google/protobuf").mkdir(parents=True)
    protoc = prefix / "bin/protoc"
    protoc.write_text("#!/bin/sh\\necho 'libprotoc 29.3'\\n")
    protoc.chmod(0o755)
"""


class InstallProtoc(unittest.TestCase):
    def run_script(self, arch, fail=""):
        temp = tempfile.TemporaryDirectory(prefix="drasi protoc-")
        self.addCleanup(temp.cleanup)
        root = Path(temp.name)
        binaries = root / "bin"
        binaries.mkdir()
        for command in ("uname", "curl", "sha256sum", "unzip"):
            binary = binaries / command
            binary.write_text(FAKE_COMMAND)
            binary.chmod(0o755)
        calls_file = root / "calls"
        scratch = root / "scratch"
        scratch.mkdir()
        prefix = root / "installed"
        env = os.environ.copy()
        env.update(
            PATH=f"{binaries}{os.pathsep}{env['PATH']}",
            TMPDIR=str(scratch),
            TEST_HOST_ARCH=arch,
            TEST_FAIL=fail,
            TEST_CALLS=str(calls_file),
        )
        result = subprocess.run(
            ["bash", str(SCRIPT), str(prefix)],
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )
        calls = [json.loads(line) for line in calls_file.read_text().splitlines()]
        self.assertEqual(list(scratch.iterdir()), [], "download directory leaked")
        return prefix, result, calls

    def test_installs_matching_compiler_and_headers_for_each_host(self):
        for host, asset in (
            ("x86_64", "x86_64"),
            ("aarch64", "aarch_64"),
            ("arm64", "aarch_64"),
        ):
            with self.subTest(host=host):
                prefix, result, calls = self.run_script(host)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("libprotoc 29.3", result.stdout)
                self.assertTrue((prefix / "bin/protoc").is_file())
                self.assertTrue((prefix / "include/google/protobuf").is_dir())
                self.assertEqual(
                    [call[0] for call in calls],
                    ["uname", "curl", "sha256sum", "unzip"],
                )
                curl_args = calls[1][1]
                self.assertIn("--fail", curl_args)
                self.assertIn(
                    "https://github.com/protocolbuffers/protobuf/releases/download/"
                    f"v29.3/protoc-29.3-linux-{asset}.zip",
                    curl_args,
                )
                self.assertEqual(calls[2][1], ["--check", "-"])
                self.assertEqual(
                    calls[2][2],
                    f"{CHECKSUMS[asset]}  {curl_args[curl_args.index('--output') + 1]}\n",
                )

    def test_rejects_unsupported_host_before_downloading(self):
        prefix, result, calls = self.run_script("riscv64")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unsupported protoc host architecture: riscv64", result.stderr)
        self.assertTrue(all(call[0] == "uname" for call in calls))
        self.assertFalse(prefix.exists())

    def test_download_or_checksum_failure_never_unpacks(self):
        for command in ("curl", "sha256sum"):
            with self.subTest(command=command):
                prefix, result, calls = self.run_script("x86_64", fail=command)
                self.assertEqual(result.returncode, 17)
                self.assertIn(f"{command} failed", result.stderr)
                self.assertNotIn("unzip", [call[0] for call in calls])
                self.assertFalse(prefix.exists())

    def test_extraction_failure_is_reported(self):
        _, result, _ = self.run_script("x86_64", fail="unzip")
        self.assertEqual(result.returncode, 17)
        self.assertIn("unzip failed", result.stderr)


if __name__ == "__main__":
    unittest.main()
