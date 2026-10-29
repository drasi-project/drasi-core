# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.

"""Install the pinned official protoc archive without changing system packages."""

import argparse
from contextlib import ExitStack
import hashlib
import io
from pathlib import Path, PurePosixPath
import platform
import shutil
import stat
import subprocess
from urllib.request import Request, urlopen
import zipfile


VERSION = "36.2"
# Release asset digests: https://github.com/protocolbuffers/protobuf/releases/tag/v36.2
ARCHIVES = {
    ("Linux", "x86_64"): (
        "linux-x86_64",
        "121f6c7afe1d4d0e3ea6aab9432038599250134cbf4474cb1167d2c7decd4278",
    ),
    ("Linux", "aarch64"): (
        "linux-aarch_64",
        "8b8f18bd2b30346efbc698dd5a73dd7c805f3ef8380f6dfc95c768f3f1852f6a",
    ),
    ("Darwin", "x86_64"): (
        "osx-x86_64",
        "228cc7add4616cc14ca5e80dee83209d44449a7aee95a914ae748fa374efb078",
    ),
    ("Darwin", "aarch64"): (
        "osx-aarch_64",
        "9cd98a532c5c5e0c4161314de0225de27e4c8a323917b6ea7b1b714d3ae23466",
    ),
    ("Windows", "x86_64"): (
        "win64",
        "f0c128dc0d8492eceece83bb459a4c0e316764b929ffbf1aa416357fd644edd3",
    ),
}
MAX_ARCHIVE_BYTES = 64 * 1024 * 1024
MAX_EXTRACTED_BYTES = 128 * 1024 * 1024


def archive_for(system, machine):
    architecture = {"amd64": "x86_64", "arm64": "aarch64"}.get(
        machine.lower(), machine.lower()
    )
    try:
        suffix, checksum = ARCHIVES[(system, architecture)]
    except KeyError as error:
        raise ValueError(f"Unsupported protoc platform: {system}/{machine}") from error
    return f"protoc-{VERSION}-{suffix}.zip", checksum


def download_archive(url):
    request = Request(url, headers={"User-Agent": "drasi-protoc-installer"})
    with urlopen(request, timeout=60) as response:
        data = response.read(MAX_ARCHIVE_BYTES + 1)
    if len(data) > MAX_ARCHIVE_BYTES:
        raise ValueError("protoc archive exceeds the download size limit")
    return data


def install_protoc(destination):
    destination = Path(destination).resolve()
    if destination.exists():
        raise FileExistsError(f"Refusing to overwrite protoc destination: {destination}")
    system = platform.system()
    asset, checksum = archive_for(system, platform.machine())
    url = f"https://github.com/protocolbuffers/protobuf/releases/download/v{VERSION}/{asset}"
    data = download_archive(url)
    actual = hashlib.sha256(data).hexdigest()
    if actual != checksum:
        raise ValueError(f"SHA-256 mismatch for {asset}: expected {checksum}, got {actual}")

    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        members = archive.infolist()
        if sum(member.file_size for member in members) > MAX_EXTRACTED_BYTES:
            raise ValueError("protoc archive exceeds the extracted size limit")
        for member in members:
            path = PurePosixPath(member.filename)
            if (
                path.is_absolute()
                or ".." in path.parts
                or "\\" in member.filename
                or ":" in member.filename
                or stat.S_ISLNK(member.external_attr >> 16)
            ):
                raise ValueError(f"Unsafe protoc archive entry: {member.filename}")

        with ExitStack() as cleanup:
            destination.mkdir()
            cleanup.callback(shutil.rmtree, destination)
            archive.extractall(destination)
            binary = destination / "bin" / ("protoc.exe" if system == "Windows" else "protoc")
            include = destination / "include" / "google" / "protobuf" / "descriptor.proto"
            if not binary.is_file() or not include.is_file():
                raise ValueError("protoc archive is missing its compiler or standard includes")
            if system != "Windows":
                binary.chmod(0o755)
            version = subprocess.run(
                [str(binary), "--version"],
                check=True,
                capture_output=True,
                text=True,
                timeout=30,
            ).stdout.strip()
            if version != f"libprotoc {VERSION}":
                raise ValueError(f"Unexpected protoc version: {version!r}")
            cleanup.pop_all()
    return binary


def publish_environment(binary, env_file, path_file):
    with Path(env_file).open("a", encoding="utf-8", newline="\n") as output:
        output.write(f"PROTOC={binary}\n")
        output.write(f"PROTOC_INCLUDE={binary.parent.parent / 'include'}\n")
    with Path(path_file).open("a", encoding="utf-8", newline="\n") as output:
        output.write(f"{binary.parent}\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, required=True)
    parser.add_argument("--github-env", type=Path)
    parser.add_argument("--github-path", type=Path)
    args = parser.parse_args()
    if bool(args.github_env) != bool(args.github_path):
        parser.error("--github-env and --github-path must be supplied together")
    binary = install_protoc(args.destination)
    if args.github_env is not None:
        publish_environment(binary, args.github_env, args.github_path)
    print(f"Installed and verified libprotoc {VERSION}: {binary}")


if __name__ == "__main__":
    main()
