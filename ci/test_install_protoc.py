# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.

import hashlib
import io
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile

import install_protoc


def fixture_archive(compiler="protoc", extra=()):
    data = io.BytesIO()
    with zipfile.ZipFile(data, "w") as archive:
        archive.writestr(f"bin/{compiler}", b"compiler fixture")
        archive.writestr("include/google/protobuf/descriptor.proto", b"fixture")
        for name, content in extra:
            archive.writestr(name, content)
    return data.getvalue()


class ProtocInstallerTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.destination = self.root / "installed"

    def install_fixture(self, data, system="Linux", version=None):
        checksum = hashlib.sha256(data).hexdigest()
        result = subprocess.CompletedProcess(
            [], 0, stdout=version or f"libprotoc {install_protoc.VERSION}\n"
        )
        with (
            patch.object(install_protoc.platform, "system", return_value=system),
            patch.object(install_protoc, "archive_for", return_value=("fixture.zip", checksum)),
            patch.object(install_protoc, "download_archive", return_value=data),
            patch.object(install_protoc.subprocess, "run", return_value=result) as run,
        ):
            binary = install_protoc.install_protoc(self.destination)
            run.assert_called_once_with(
                [str(binary), "--version"],
                check=True,
                capture_output=True,
                text=True,
                timeout=30,
            )
        return binary

    def test_platform_selection_and_pinned_checksums(self):
        cases = [
            ("Linux", "x86_64", "linux-x86_64"),
            ("Linux", "aarch64", "linux-aarch_64"),
            ("Darwin", "arm64", "osx-aarch_64"),
            ("Darwin", "x86_64", "osx-x86_64"),
            ("Windows", "AMD64", "win64"),
        ]
        for system, machine, suffix in cases:
            with self.subTest(system=system, machine=machine):
                asset, checksum = install_protoc.archive_for(system, machine)
                self.assertEqual(asset, f"protoc-{install_protoc.VERSION}-{suffix}.zip")
                self.assertRegex(checksum, r"^[0-9a-f]{64}$")

    def test_unsupported_platform_is_not_guessed(self):
        for system, machine in [("Windows", "ARM64"), ("Linux", "s390x"), ("Plan9", "amd64")]:
            with self.subTest(system=system, machine=machine):
                with self.assertRaisesRegex(ValueError, "Unsupported protoc platform"):
                    install_protoc.archive_for(system, machine)

    def test_installs_compiler_and_includes(self):
        binary = self.install_fixture(fixture_archive())
        self.assertEqual(binary, self.destination.resolve() / "bin" / "protoc")
        self.assertTrue((binary.parent.parent / "include/google/protobuf/descriptor.proto").is_file())
        if install_protoc.platform.system() != "Windows":
            self.assertTrue(binary.stat().st_mode & stat.S_IXUSR)

    def test_windows_uses_exe(self):
        binary = self.install_fixture(fixture_archive("protoc.exe"), system="Windows")
        self.assertEqual(binary.name, "protoc.exe")

    def test_hash_mismatch_does_not_extract_or_execute(self):
        with (
            patch.object(install_protoc, "archive_for", return_value=("fixture.zip", "0" * 64)),
            patch.object(install_protoc, "download_archive", return_value=fixture_archive()),
            patch.object(install_protoc.subprocess, "run") as run,
        ):
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                install_protoc.install_protoc(self.destination)
        self.assertFalse(self.destination.exists())
        run.assert_not_called()

    def test_existing_destination_is_preserved(self):
        self.destination.mkdir()
        sentinel = self.destination / "keep"
        sentinel.write_text("existing data")
        with patch.object(install_protoc, "download_archive") as download:
            with self.assertRaises(FileExistsError):
                install_protoc.install_protoc(self.destination)
        download.assert_not_called()
        self.assertEqual(sentinel.read_text(), "existing data")

    def test_wrong_version_removes_only_the_new_installation(self):
        sentinel = self.root / "keep"
        sentinel.write_text("existing data")
        with self.assertRaisesRegex(ValueError, "Unexpected protoc version"):
            self.install_fixture(fixture_archive(), version="libprotoc 0.1")
        self.assertFalse(self.destination.exists())
        self.assertEqual(sentinel.read_text(), "existing data")

    def test_unsafe_archive_paths_are_rejected(self):
        for path in ["../escape", "/absolute", "C:/escape", "dir\\escape"]:
            with self.subTest(path=path):
                with self.assertRaisesRegex(ValueError, "Unsafe protoc archive entry"):
                    self.install_fixture(fixture_archive(extra=[(path, b"unsafe")]))
                self.assertFalse(self.destination.exists())
        self.assertFalse((self.root / "escape").exists())

    def test_archive_symlinks_are_rejected(self):
        link = zipfile.ZipInfo("link")
        link.create_system = 3
        link.external_attr = (stat.S_IFLNK | 0o777) << 16
        with self.assertRaisesRegex(ValueError, "Unsafe protoc archive entry"):
            self.install_fixture(fixture_archive(extra=[(link, b"/outside")]))
        self.assertFalse(self.destination.exists())

    def test_missing_compiler_is_not_a_successful_installation(self):
        with self.assertRaisesRegex(ValueError, "missing its compiler"):
            self.install_fixture(fixture_archive(compiler="wrong"))
        self.assertFalse(self.destination.exists())

    def test_environment_uses_the_verified_absolute_paths(self):
        binary = self.install_fixture(fixture_archive())
        env_file, path_file = self.root / "env", self.root / "path"
        install_protoc.publish_environment(binary, env_file, path_file)
        self.assertEqual(
            env_file.read_text(),
            f"PROTOC={binary}\nPROTOC_INCLUDE={binary.parent.parent / 'include'}\n",
        )
        self.assertEqual(path_file.read_text(), f"{binary.parent}\n")


if __name__ == "__main__":
    unittest.main()
