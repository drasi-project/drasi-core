#!/usr/bin/env python3
"""Freeze, validate, package and describe one release candidate. No registry writes."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tomllib


TARGETS = {
    "x86_64-unknown-linux-gnu": "linux-amd64",
    "aarch64-unknown-linux-gnu": "linux-arm64",
    "x86_64-unknown-linux-musl": "linux-musl-amd64",
    "aarch64-unknown-linux-musl": "linux-musl-arm64",
    "x86_64-pc-windows-gnu": "windows-amd64",
    "x86_64-pc-windows-msvc": "windows-msvc-amd64",
    "x86_64-apple-darwin": "darwin-amd64",
    "aarch64-apple-darwin": "darwin-arm64",
}
TEST_REGISTRY = "ghcr.io/drasi-project/drasi-core-nightly"
IDENTITY = ("https://github.com/drasi-project/drasi-core/"
            ".github/workflows/publish-plugins.yml@refs/heads/main")
ISSUER = "https://token.actions.githubusercontent.com"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def run(*args, cwd=None):
    return subprocess.check_output(args, cwd=cwd, text=True).strip()


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def output(name, value):
    print(f"{name}={value}")
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
            stream.write(f"{name}={value}\n")


def publication_context(inputs, env):
    repository = "drasi-project/drasi-core"
    ref = env.get("GITHUB_REF", "")
    require(env.get("GITHUB_REPOSITORY") == repository,
            "Publication and repair require the official repository")
    if inputs.get("allow_branch_test", False) is True:
        require(env.get("GITHUB_EVENT_NAME") == "workflow_dispatch"
                and ref.startswith("refs/heads/")
                and env.get("GITHUB_WORKFLOW_REF") ==
                f"{repository}/.github/workflows/nightly.yml@{ref}",
                "Branch testing requires a manually dispatched nightly branch")
        require(inputs.get("registry") == TEST_REGISTRY
                and inputs.get("tag") ==
                f"nightly-{env['GITHUB_RUN_ID']}-{env['GITHUB_RUN_ATTEMPT']}"
                and inputs.get("candidate_artifact") ==
                f"nightly-candidate-{env['GITHUB_RUN_ID']}-{env['GITHUB_RUN_ATTEMPT']}"
                and re.fullmatch(r"[0-9a-f]{64}", inputs.get("candidate_sha256", ""))
                and inputs.get("sign") is True
                and inputs.get("dry_run") is False
                and inputs.get("skip_visibility") is False
                and not inputs.get("ref")
                and not inputs.get("pre_release")
                and inputs.get("repair_batch", "none") == "none"
                and inputs.get("repair_action", "plan") == "plan",
                "Branch testing requires the complete signed, isolated nightly candidate")
    else:
        require(ref == "refs/heads/main",
                "Publication and repair must use the trusted main workflow")
    return f"https://github.com/{repository}/.github/workflows/publish-plugins.yml@{ref}"


def candidate_identity(candidate):
    ref = candidate["source_ref"]
    require(ref.startswith("refs/heads/"), "Candidate must come from a branch")
    require(ref == "refs/heads/main" or candidate["branch_test"] is True,
            "Non-main candidate requires explicit branch-test opt-in")
    return IDENTITY.removesuffix("refs/heads/main") + ref


def metadata(root):
    require((root / "Cargo.lock").is_file(),
            "Missing required Cargo.lock; restore the committed candidate lockfile")
    before = sha(root / "Cargo.lock")
    result = json.loads(run("cargo", "metadata", "--locked", "--format-version", "1", cwd=root))
    require(sha(root / "Cargo.lock") == before, "Cargo changed the candidate lockfile")
    return result


def inventory(data, config):
    members = set(data["workspace_members"])
    packages = [p for p in data["packages"] if p["id"] in members]
    overrides = {p["name"]: p for p in config.get("package", [])}
    defaults = config.get("workspace", {})
    crates = []
    plugins = []
    for package in packages:
        name = package["name"]
        policy = defaults | overrides.get(name, {})
        if (package.get("publish") != [] and policy.get("publish", True)
                and policy.get("release", True)):
            crates.append({"name": name, "version": package["version"]})
        match = re.fullmatch(r"drasi-(source|reaction|bootstrap|identity|secret-store)-(.+)", name)
        if match and "dynamic-plugin" in package["features"]:
            plugins.append({
                "name": name, "version": package["version"],
                "type": match[1], "kind": match[2],
            })
    versions = {p["name"]: p["version"] for p in packages}
    return sorted(crates, key=lambda p: p["name"]), sorted(plugins, key=lambda p: p["name"]), versions


def freeze(root, destination):
    root = root.resolve()
    require(not destination.exists(), f"Candidate output already exists: {destination}")
    data = metadata(root)
    config = tomllib.loads((root / "release-plz.toml").read_text(encoding="utf-8"))
    crates, plugins, versions = inventory(data, config)
    require(crates and plugins, "Candidate must contain release crates and plugins")
    files = set(run("git", "ls-files", "-z", cwd=root).split("\0")) | {"Cargo.lock"}
    # release-plz can create the first changelog for a newly released package.
    for package in data["packages"]:
        if package["id"] in data["workspace_members"]:
            changelog = Path(package["manifest_path"]).parent / "CHANGELOG.md"
            if changelog.is_file():
                files.add(changelog.relative_to(root).as_posix())
    destination.mkdir(parents=True)
    archive = destination / "source.tar"
    with tarfile.open(archive, "w") as bundle:
        for name in sorted(files):
            if not name:
                continue
            path = root / name
            require(path.is_file() or path.is_symlink(), f"Missing candidate source: {name}")
            bundle.add(path, arcname=name, recursive=False)
    lock_hash = sha(root / "Cargo.lock")
    baseline = subprocess.check_output(["git", "show", "HEAD:Cargo.lock"], cwd=root)
    candidate = {
        "schema_version": 1,
        "source_commit": run("git", "rev-parse", "HEAD", cwd=root),
        "source_ref": os.environ.get("GITHUB_REF", "refs/heads/main"),
        "branch_test": os.environ.get("ALLOW_BRANCH_TEST") == "true",
        "source_sha256": sha(archive),
        "lock_sha256": lock_hash,
        "baseline_lock_sha256": hashlib.sha256(baseline).hexdigest(),
        "mode": "locked",
        "repository": os.environ.get("GITHUB_REPOSITORY", "drasi-project/drasi-core"),
        "run_id": os.environ.get("GITHUB_RUN_ID", "local"),
        "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT", "local"),
        "toolchain": tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]["channel"],
        "crates": crates, "plugins": plugins, "versions": versions,
        "targets": TARGETS,
    }
    write_json(destination / "candidate.json", candidate)
    output("candidate_sha256", sha(destination / "candidate.json"))
    output("lock_sha256", lock_hash)


def load(bundle, expected):
    require(re.fullmatch(r"[0-9a-f]{64}", expected), "Expected candidate SHA-256 is required")
    require(sha(bundle / "candidate.json") == expected, "Candidate manifest hash mismatch")
    candidate = json.loads((bundle / "candidate.json").read_text())
    require(candidate["schema_version"] == 1, "Unsupported candidate schema")
    require(candidate["repository"] == "drasi-project/drasi-core", "Unexpected producer repository")
    require(sha(bundle / "source.tar") == candidate["source_sha256"], "Candidate source hash mismatch")
    return candidate


def restore(bundle, expected, root):
    candidate = load(bundle, expected)
    require(not root.exists(), f"Restore destination must not exist: {root}")
    root.mkdir(parents=True)
    with tarfile.open(bundle / "source.tar") as archive:
        archive.extractall(root, filter="data")
    check(bundle, expected, root)
    return candidate


def check(bundle, expected, root):
    candidate = load(bundle, expected)
    require((root / "Cargo.lock").is_file(), "Missing restored Cargo.lock")
    require(sha(root / "Cargo.lock") == candidate["lock_sha256"], "Candidate lock hash mismatch")
    with tarfile.open(bundle / "source.tar") as archive:
        for member in archive:
            path = root / member.name
            if member.isfile():
                require(path.is_file() and path.read_bytes() == archive.extractfile(member).read(),
                        f"Prepared source mismatch: {member.name}")
            elif member.issym():
                require(path.is_symlink() and os.readlink(path) == member.linkname,
                        f"Prepared symlink mismatch: {member.name}")
            else:
                require(member.isdir(), f"Unsupported candidate member: {member.name}")
    print(f"Verified candidate {candidate['source_sha256']} lock {candidate['lock_sha256']}")
    return candidate


def package(bundle, expected, root, destination):
    candidate = check(bundle, expected, root)
    metadata(root)
    # Restored, hash-checked sources are intentionally uncommitted under the tooling checkout.
    args = ["cargo", "package", "--locked", "--allow-dirty",
            "--target-dir", str((root / "target").resolve())]
    for crate in candidate["crates"]:
        args.extend(["--package", crate["name"]])
    subprocess.run(args, cwd=root, check=True)
    archives = []
    for crate in candidate["crates"]:
        filename = f"{crate['name']}-{crate['version']}.crate"
        path = root / "target" / "package" / filename
        require(path.is_file(), f"Missing verified package {filename}")
        archives.append({"name": filename, "sha256": sha(path)})
    check(bundle, expected, root)
    write_json(destination, {"candidate_sha256": expected, "archives": archives})


def registry_packages(registry, candidate=None):
    require(re.fullmatch(r"ghcr\.io/[a-z0-9-]+(?:/[a-z0-9._-]+)*", registry),
            "Expected a GHCR namespace without credentials or a tag")
    prefix = registry.split("/", 2)[2] + "/" if registry.count("/") > 1 else ""
    names = [prefix + "drasi-plugin-directory"]
    if candidate:
        names += [prefix + p["type"] + "/" + p["kind"] for p in candidate["plugins"]]
    return sorted(set(names))


def target_report(bundle, expected, root, publication, target, destination):
    candidate = check(bundle, expected, root)
    require(target in candidate["targets"], "Unexpected target")
    report = json.loads(publication.read_text())
    validate_report(candidate, report, target)
    report.update({"candidate_sha256": expected, "lock_sha256": candidate["lock_sha256"],
                   "target": target, "source_sha256": candidate["source_sha256"]})
    write_json(destination, report)


def validate_report(candidate, report, target):
    require(report["schema_version"] == 1, "Unsupported publication report")
    require(report["artifact_failures"] == 0 and report["catalog_failures"] == 0,
            "Required publication operation failed")
    require(report["catalog_successes"] == len(candidate["plugins"]), "Incomplete catalog")
    by_name = {a["name"]: a for a in report["artifacts"]}
    require(len(by_name) == len(report["artifacts"])
            and set(by_name) == {p["name"] for p in candidate["plugins"]},
            "Incomplete or duplicate plugin inventory")
    for plugin in candidate["plugins"]:
        artifact = by_name[plugin["name"]]
        require(all(artifact[k] == plugin[k] for k in ("name", "version", "type", "kind"))
                and artifact["target"] == target, "Plugin identity mismatch")
        for field, crate in (("sdk_version", "drasi-plugin-sdk"),
                             ("core_version", "drasi-core"), ("lib_version", "drasi-lib")):
            require(artifact[field] == candidate["versions"][crate], "Plugin ABI version mismatch")
        repository = artifact["reference"].rsplit(":", 1)[0]
        require(re.fullmatch(re.escape(repository) + r"@sha256:[0-9a-f]{64}",
                             artifact["digest_reference"]), "Missing immutable OCI digest")
    return by_name


def handoff(bundle, expected, reports_dir, packages, destination):
    candidate = load(bundle, expected)
    require(candidate["run_id"] == os.environ.get("GITHUB_RUN_ID")
            and candidate["run_attempt"] == os.environ.get("GITHUB_RUN_ATTEMPT"),
            "Handoff must belong to this producer run and attempt")
    package_report = json.loads(packages.read_text())
    require(package_report["candidate_sha256"] == expected, "Packaging candidate mismatch")
    expected_crates = {f"{p['name']}-{p['version']}.crate" for p in candidate["crates"]}
    require({p["name"] for p in package_report["archives"]} == expected_crates
            and len(package_report["archives"]) == len(expected_crates)
            and all(re.fullmatch(r"[0-9a-f]{64}", p["sha256"]) for p in package_report["archives"]),
            "Incomplete package inventory")
    reports = [json.loads(path.read_text()) for path in sorted(reports_dir.glob("*.json"))]
    require(len(reports) == len(TARGETS)
            and {r["target"] for r in reports} == set(TARGETS), "Incomplete target inventory")
    tag = f"nightly-{candidate['run_id']}-{candidate['run_attempt']}"
    artifacts = []
    for report in reports:
        require(report["candidate_sha256"] == expected
                and report["lock_sha256"] == candidate["lock_sha256"]
                and report["source_sha256"] == candidate["source_sha256"],
                "Target candidate or lock mismatch")
        by_name = validate_report(candidate, report, report["target"])
        for plugin in candidate["plugins"]:
            artifact = by_name[plugin["name"]]
            repository = f"{TEST_REGISTRY}/{plugin['type']}/{plugin['kind']}"
            require(artifact["reference"] == f"{repository}:{tag}-{TARGETS[report['target']]}"
                    and re.fullmatch(re.escape(repository) + r"@sha256:[0-9a-f]{64}",
                                     artifact["digest_reference"]),
                    "Unexpected namespace, tag or immutable digest")
            require(artifact["signed"] is True and artifact["verified"] is True,
                    "Unsigned or unverified plugin")
            artifacts.append(artifact)
    result = candidate | {
        "candidate_sha256": expected,
        "candidate_artifact": f"nightly-candidate-{candidate['run_id']}-{candidate['run_attempt']}",
        "run_url": f"https://github.com/{candidate['repository']}/actions/runs/{candidate['run_id']}",
        "registry": TEST_REGISTRY, "tag": tag,
        "certificate_identity": candidate_identity(candidate), "oidc_issuer": ISSUER,
        "artifacts": artifacts, "packages": package_report["archives"],
    }
    write_json(destination, result)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["freeze", "restore", "check", "package", "packages",
                                             "target-report", "handoff", "publication-context"])
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--bundle", type=Path)
    parser.add_argument("--sha256")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--registry", default="ghcr.io/drasi-project")
    parser.add_argument("--report", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--reports", type=Path)
    parser.add_argument("--packages", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "publication-context":
            output("certificate_identity",
                   publication_context(json.loads(os.environ["WORKFLOW_INPUTS"]), os.environ))
        elif args.command == "freeze":
            freeze(args.root.resolve(), args.output.resolve())
        elif args.command == "restore":
            restore(args.bundle, args.sha256, args.root)
        elif args.command == "check":
            check(args.bundle, args.sha256, args.root)
        elif args.command == "package":
            package(args.bundle, args.sha256, args.root, args.output)
        elif args.command == "packages":
            candidate = load(args.bundle, args.sha256) if args.bundle else None
            print("\n".join(registry_packages(args.registry, candidate)))
        elif args.command == "target-report":
            target_report(args.bundle, args.sha256, args.root, args.report, args.target, args.output)
        elif args.command == "handoff":
            handoff(args.bundle, args.sha256, args.reports, args.packages, args.output)
    except (ValueError, OSError, KeyError, tarfile.TarError, subprocess.CalledProcessError) as error:
        print(f"::error::Release candidate: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
