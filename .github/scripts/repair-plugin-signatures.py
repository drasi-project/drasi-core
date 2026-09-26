#!/usr/bin/env python3
"""Sign only reviewed, existing plugin digests from the official main workflow."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlencode


REPOSITORY = "drasi-project/drasi-core"
WORKFLOW_REF = f"{REPOSITORY}/.github/workflows/publish-plugins.yml@refs/heads/main"
IDENTITY = f"https://github.com/{WORKFLOW_REF}"
ISSUER = "https://token.actions.githubusercontent.com"
COSIGN_VERSION = "v2.5.2"
INVENTORY = Path(__file__).resolve().parents[1] / "plugin-signature-repairs.json"
ATTEMPTS = 3
RETRY_DELAY = 2
HTTP_TIMEOUT = 30
COSIGN_TIMEOUT = 120
JSON_LIMIT = 1024 * 1024
BINARY_LIMIT = 32 * 1024 * 1024
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")
MANIFEST_TYPE = "application/vnd.oci.image.manifest.v1+json"
INDEX_TYPE = "application/vnd.oci.image.index.v1+json"
MEDIA_PREFIX = "application/vnd.drasi.plugin.v1+"
CONFIG = {
    "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
    "size": 2,
    "mediaType": MEDIA_PREFIX + "config",
}
ANNOTATIONS = {
    "org.opencontainers.image.title": "name",
    "org.opencontainers.image.version": "version",
    "io.drasi.plugin.kind": "kind",
    "io.drasi.plugin.type": "type",
    "io.drasi.plugin.sdk-version": "sdk_version",
    "io.drasi.plugin.core-version": "core_version",
    "io.drasi.plugin.lib-version": "lib_version",
    "io.drasi.plugin.target-triple": "target_triple",
}
PUBLISH_DEFAULTS = {
    "pre_release": "",
    "tag": "",
    "registry": "ghcr.io/drasi-project",
    "sign": True,
    "dry_run": False,
    "skip_visibility": False,
    "ref": "",
}


class RepairError(Exception):
    """A fail-closed error safe to include in the public report."""


def require(condition, message):
    if not condition:
        raise RepairError(message)


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate JSON key")
        result[key] = value
    return result


def parse_json(data, label):
    try:
        return json.loads(data, object_pairs_hook=unique_object)
    except (ValueError, UnicodeDecodeError):
        raise RepairError(f"{label}: invalid JSON") from None


def keys(value, expected, label):
    require(isinstance(value, dict) and set(value) == set(expected),
            f"{label}: unexpected fields")


def load_batch(name):
    try:
        inventory = parse_json(INVENTORY.read_bytes(), "repair inventory")
    except OSError:
        raise RepairError("Cannot read the checked-in repair inventory") from None
    keys(inventory, ("schema_version", "batches"), "repair inventory")
    require(type(inventory["schema_version"]) is int and inventory["schema_version"] == 1
            and isinstance(inventory["batches"], dict),
            "Unsupported repair inventory schema")
    require(isinstance(name, str) and name in inventory["batches"],
            "Unsupported repair batch; select a reviewed workflow choice")
    batch = inventory["batches"][name]
    keys(batch, ("provenance", "artifacts"), "repair batch")
    provenance = batch["provenance"]
    keys(provenance, (
        "repository", "source_commit", "run_id", "run_path", "run_event", "job_id",
        "job_name", "job_workflow_ref", "ffi_sdk_version", "ffi_source_sha256",
        "reviewed_at",
    ), "release provenance")
    require(provenance["repository"] == REPOSITORY
            and provenance["job_workflow_ref"] == WORKFLOW_REF
            and provenance["run_path"] == ".github/workflows/release-plz.yml"
            and provenance["run_event"] == "workflow_dispatch"
            and provenance["job_name"] == "Publish plugins / Build & Publish (darwin-arm64)",
            "Unsupported release provenance")
    require(isinstance(provenance["source_commit"], str)
            and re.fullmatch(r"[0-9a-f]{40}", provenance["source_commit"])
            and isinstance(provenance["ffi_source_sha256"], str)
            and re.fullmatch(r"[0-9a-f]{64}", provenance["ffi_source_sha256"])
            and isinstance(provenance["ffi_sdk_version"], str)
            and VERSION.fullmatch(provenance["ffi_sdk_version"]),
            "Invalid source/ABI pin in repair inventory")
    for field in ("run_id", "job_id"):
        require(type(provenance[field]) is int and provenance[field] > 0,
                f"Invalid {field} in repair inventory")
    artifacts = batch["artifacts"]
    require(isinstance(artifacts, list) and artifacts, "Empty repair inventory")
    references = set()
    for artifact in artifacts:
        keys(artifact, (
            "repository", "manifest_digest", "platform", "release_tag",
            "published_at", "binary", "metadata",
        ), "repair artifact")
        require(isinstance(artifact["manifest_digest"], str)
                and DIGEST.fullmatch(artifact["manifest_digest"]),
                "Repair requires an immutable sha256 manifest digest")
        for layer in ("binary", "metadata"):
            descriptor = artifact[layer]
            expected = ("digest", "size", "fields") if layer == "metadata" else ("digest", "size")
            keys(descriptor, expected, f"{layer} descriptor")
            limit = JSON_LIMIT if layer == "metadata" else BINARY_LIMIT
            require(isinstance(descriptor["digest"], str)
                    and DIGEST.fullmatch(descriptor["digest"])
                    and type(descriptor["size"]) is int and 0 < descriptor["size"] <= limit,
                    f"Invalid {layer} digest/size in repair inventory")
        fields = artifact["metadata"]["fields"]
        keys(fields, (*ANNOTATIONS.values(), "description", "license"), "plugin metadata")
        require(all(isinstance(value, str) for value in fields.values()),
                "Plugin metadata fields must be strings")
        require(re.fullmatch(r"[a-z][a-z0-9-]*", fields["kind"])
                and fields["type"] == "reaction"
                and fields["name"] == f"drasi-reaction-{fields['kind']}"
                and artifact["repository"] == f"drasi-project/reaction/{fields['kind']}",
                "Repair only accepts reviewed drasi-project reaction repositories")
        require(all(VERSION.fullmatch(fields[field]) for field in (
            "version", "sdk_version", "core_version", "lib_version",
        )), "Invalid plugin version in repair inventory")
        require(fields["target_triple"] == "aarch64-apple-darwin"
                and artifact["platform"] == "darwin/arm64"
                and artifact["release_tag"] == fields["version"] + "-darwin-arm64",
                "Unsupported repair platform or release tag")
        ref = reference(artifact)
        require(ref not in references, "Duplicate repair artifact")
        references.add(ref)
    return batch


def selection():
    inputs = parse_json(os.environ.get("WORKFLOW_INPUTS", ""), "workflow inputs")
    require(isinstance(inputs, dict), "Workflow inputs must be an object")
    repair_keys = {"repair_batch", "repair_action"}
    require(not (set(inputs) - set(PUBLISH_DEFAULTS) - repair_keys),
            "Unsupported workflow input")
    if not (set(inputs) & repair_keys):
        return "publish", None, None  # Existing workflow_call contract.
    require(repair_keys <= set(inputs), "Incomplete repair selection")
    name, action = inputs["repair_batch"], inputs["repair_action"]
    if name == "none" and action == "plan":
        return "publish", None, None
    require(action in ("plan", "execute") and name != "none",
            "Repair requires a reviewed batch and plan or execute action")
    for field, default in PUBLISH_DEFAULTS.items():
        require(inputs.get(field, default) == default
                and type(inputs.get(field, default)) is type(default),
                f"Repair does not accept the publication input override: {field}")
    for variable, expected in (
        ("GITHUB_REPOSITORY", REPOSITORY),
        ("GITHUB_REF", "refs/heads/main"),
        ("GITHUB_EVENT_NAME", "workflow_dispatch"),
        ("GITHUB_WORKFLOW_REF", WORKFLOW_REF),
        ("GITHUB_ACTIONS", "true"),
    ):
        require(os.environ.get(variable) == expected,
                f"Repair requires the official main workflow_dispatch context: {variable}")
    sha = os.environ.get("GITHUB_SHA", "")
    require(re.fullmatch(r"[0-9a-f]{40}", sha)
            and os.environ.get("GITHUB_WORKFLOW_SHA") == sha,
            "Repair requires matching main event and workflow commit SHAs")
    return action, name, load_batch(name)


def reference(artifact):
    return f"ghcr.io/{artifact['repository']}@{artifact['manifest_digest']}"


def isolated_env(directory):
    directory.mkdir()
    # Do not inherit registry credentials, private keys, alternate Sigstore
    # endpoints/trust roots, credential helpers, proxies, or cosign overrides.
    return {
        "PATH": os.environ.get("PATH", os.defpath),
        "HOME": str(directory),
        "DOCKER_CONFIG": str(directory / "docker"),
        "NO_COLOR": "1",
    }


class Commands:
    def __init__(self):
        self.attempts = []

    def run(self, argv, env, timeout, label, stdin=None):
        try:
            result = subprocess.run(
                argv, input=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                env=env, timeout=timeout, check=False,
            )
        except subprocess.TimeoutExpired:
            self.attempts.append({"operation": label, "result": "timeout"})
            return None
        except OSError:
            raise RepairError(f"{label}: cannot start required command") from None
        self.attempts.append({"operation": label, "exit_code": result.returncode})
        return result

    @staticmethod
    def wait(attempt):
        if attempt < ATTEMPTS:
            time.sleep(RETRY_DELAY)


class PublicRegistry:
    def __init__(self, directory, commands):
        self.directory = directory
        self.commands = commands
        self.env = isolated_env(directory / "http")
        self.tokens = {}

    def get(self, url, label, accept="application/json", token=None, limit=JSON_LIMIT):
        config = "header = " + json.dumps("Accept: " + accept) + "\n"
        if token is not None:
            config += "header = " + json.dumps("Authorization: Bearer " + token) + "\n"
        # curl's normal cross-host redirect handling strips Authorization.
        with tempfile.NamedTemporaryFile(dir=self.directory) as body:
            for attempt in range(1, ATTEMPTS + 1):
                result = self.commands.run([
                    "curl", "--disable", "--silent", "--show-error", "--location",
                    "--proto", "=https", "--proto-redir", "=https", "--max-redirs", "3",
                    "--connect-timeout", "10", "--max-time", str(HTTP_TIMEOUT),
                    "--max-filesize", str(limit), "--output", body.name,
                    "--write-out", "%{http_code}\n%{content_type}", "--config", "-", url,
                ], self.env, HTTP_TIMEOUT + 5, label, config.encode())
                if result is not None and result.returncode == 0:
                    try:
                        code, content_type = result.stdout.decode().split("\n", 1)
                        status = int(code)
                    except (ValueError, UnicodeDecodeError):
                        raise RepairError(f"{label}: invalid HTTP status") from None
                    if status not in (429, 500, 502, 503, 504):
                        body.seek(0)
                        data = body.read(limit + 1)
                        require(len(data) <= limit, f"{label}: response exceeds size limit")
                        return status, content_type.split(";")[0], data
                    error = f"HTTP {status}"
                else:
                    error = "timeout" if result is None else f"curl exit {result.returncode}"
                print(f"{label}: {error} (attempt {attempt}/{ATTEMPTS})", file=sys.stderr)
                self.commands.wait(attempt)
        raise RepairError(f"{label}: request failed after {ATTEMPTS} attempts ({error})")

    def public_json(self, url, label):
        status, content_type, data = self.get(url, label)
        require(status == 200, f"{label}: expected HTTP 200, got {status}")
        require(content_type == "application/json", f"{label}: unexpected content type")
        return parse_json(data, label)

    def pull(self, repository, path, label, limit=JSON_LIMIT):
        if repository not in self.tokens:
            query = urlencode({"service": "ghcr.io", "scope": f"repository:{repository}:pull"})
            response = self.public_json("https://ghcr.io/token?" + query, "anonymous pull token")
            token = response.get("token") if isinstance(response, dict) else None
            require(isinstance(token, str) and 0 < len(token) <= 16384
                    and re.fullmatch(r"[A-Za-z0-9._~+/=-]+", token),
                    "Invalid anonymous GHCR pull token response")
            self.tokens[repository] = token
        return self.get(
            f"https://ghcr.io/v2/{repository}/{path}", label,
            f"{MANIFEST_TYPE}, {INDEX_TYPE}, application/octet-stream",
            self.tokens[repository], limit,
        )

    def blob(self, repository, descriptor, label):
        status, _, data = self.pull(
            repository, "blobs/" + descriptor["digest"], label, descriptor["size"] + 1,
        )
        require(status == 200, f"{label}: expected HTTP 200, got {status}")
        require(len(data) == descriptor["size"], f"{label}: size mismatch")
        require("sha256:" + hashlib.sha256(data).hexdigest() == descriptor["digest"],
                f"{label}: checksum mismatch")
        return data

    def verify_provenance(self, provenance):
        base = f"https://api.github.com/repos/{REPOSITORY}/actions"
        run = self.public_json(f"{base}/runs/{provenance['run_id']}", "original release run")
        expected = {
            "id": provenance["run_id"], "head_sha": provenance["source_commit"],
            "head_branch": "main", "path": provenance["run_path"],
            "event": provenance["run_event"], "status": "completed",
        }
        require(isinstance(run, dict) and all(run.get(k) == v for k, v in expected.items())
                and isinstance(run.get("repository"), dict)
                and run["repository"].get("full_name") == REPOSITORY,
                "Original release run provenance mismatch")
        job = self.public_json(f"{base}/jobs/{provenance['job_id']}", "original release job")
        expected = {
            "id": provenance["job_id"], "run_id": provenance["run_id"],
            "name": provenance["job_name"], "status": "completed",
        }
        require(isinstance(job, dict) and all(job.get(k) == v for k, v in expected.items()),
                "Original release job provenance mismatch")
        url = (f"https://raw.githubusercontent.com/{REPOSITORY}/{provenance['source_commit']}"
               "/components/plugin-sdk/src/ffi/metadata.rs")
        status, _, source = self.get(url, "source-bound FFI ABI", accept="text/plain")
        require(status == 200, f"Source-bound FFI ABI: expected HTTP 200, got {status}")
        require(hashlib.sha256(source).hexdigest() == provenance["ffi_source_sha256"]
                and (f'pub const FFI_SDK_VERSION: &str = "{provenance["ffi_sdk_version"]}";'
                     ).encode() in source,
                "Source-bound FFI ABI mismatch")

    def validate_artifact(self, artifact):
        repository = artifact["repository"]
        status, content_type, raw = self.pull(
            repository, "manifests/" + artifact["manifest_digest"], "artifact manifest",
        )
        require(status == 200, f"Artifact manifest: expected HTTP 200, got {status}")
        require(content_type == MANIFEST_TYPE, "Artifact manifest: unexpected content type")
        require("sha256:" + hashlib.sha256(raw).hexdigest() == artifact["manifest_digest"],
                "Artifact manifest: digest mismatch")
        fields = artifact["metadata"]["fields"]
        expected = {
            "schemaVersion": 2,
            "annotations": {key: fields[field] for key, field in ANNOTATIONS.items()},
            "config": CONFIG,
            "layers": [
                {**artifact["binary"], "mediaType": MEDIA_PREFIX + "binary"},
                {k: artifact["metadata"][k] for k in ("digest", "size")}
                | {"mediaType": MEDIA_PREFIX + "metadata"},
            ],
        }
        require(parse_json(raw, "artifact manifest") == expected,
                "Artifact manifest: metadata, descriptors or source annotations mismatch")
        require(self.blob(repository, CONFIG, "plugin config") == b"{}",
                "Plugin config mismatch")
        metadata = self.blob(repository, artifact["metadata"], "plugin metadata")
        require(parse_json(metadata, "plugin metadata") == fields, "Plugin metadata mismatch")
        binary = self.blob(repository, artifact["binary"], "plugin binary")
        # Check the immutable payload is a thin arm64 Mach-O dylib, not merely
        # an annotation claiming that target. Never load/execute the plugin.
        require(len(binary) >= 32
                and struct.unpack("<IIII", binary[:16]) == (0xFEEDFACF, 0x100000C, 0, 6),
                "Plugin binary: expected aarch64-apple-darwin Mach-O dylib")
        command_count, command_bytes = struct.unpack_from("<II", binary, 16)
        offset, end = 32, 32 + command_bytes
        require(end <= len(binary), "Plugin binary: truncated Mach-O load commands")
        platforms = []
        for _ in range(command_count):
            require(offset + 8 <= end, "Plugin binary: truncated Mach-O load command")
            command, size = struct.unpack_from("<II", binary, offset)
            require(size >= 8 and offset + size <= end,
                    "Plugin binary: invalid Mach-O load command size")
            if command == 0x32:  # LC_BUILD_VERSION; platform 1 is macOS, not iOS.
                require(size >= 24, "Plugin binary: truncated LC_BUILD_VERSION")
                platforms.append(struct.unpack_from("<I", binary, offset + 8)[0])
            offset += size
        require(offset == end and platforms == [1],
                "Plugin binary: expected exactly one macOS build platform")

    def require_absent(self, artifact):
        digest = artifact["manifest_digest"]
        tag = "sha256-" + digest.removeprefix("sha256:")
        locations = (
            "manifests/" + tag + ".sig", "manifests/" + tag, "referrers/" + digest,
        )
        for path in locations:
            status, content_type, raw = self.pull(
                artifact["repository"], path, "signature absence check",
            )
            if status == 404 and content_type == "application/json":
                body = parse_json(raw, "signature absence response")
                errors = body.get("errors") if isinstance(body, dict) else None
                require(isinstance(errors, list) and len(errors) == 1
                        and isinstance(errors[0], dict)
                        and errors[0].get("code") == "MANIFEST_UNKNOWN",
                        "Signature absence check: not a registry MANIFEST_UNKNOWN response")
            elif status == 200 and path.startswith("referrers/"):
                require(content_type == INDEX_TYPE and parse_json(raw, "referrers index") == {
                    "schemaVersion": 2, "mediaType": INDEX_TYPE, "manifests": [],
                }, "Existing or malformed OCI referrers: refusing to sign")
            else:
                raise RepairError(
                    f"Signature absence check: HTTP {status}; existing signatures, "
                    "authentication errors and unexpected responses require investigation"
                )


class Cosign:
    def __init__(self, directory, commands):
        self.commands = commands
        self.anonymous = isolated_env(directory / "verify")
        self.signing = isolated_env(directory / "sign")

    def call(self, arguments, env, label, stdin=None):
        return self.commands.run(
            ["cosign", "--timeout", f"{COSIGN_TIMEOUT}s", *arguments],
            env, COSIGN_TIMEOUT + 5, label, stdin,
        )

    def check_version(self):
        result = self.call(["version", "--json"], self.anonymous, "cosign version")
        require(result is not None and result.returncode == 0, "Cannot read cosign version")
        version = parse_json(result.stdout, "cosign version")
        require(isinstance(version, dict) and version.get("gitVersion") == COSIGN_VERSION,
                f"Repair requires pinned cosign {COSIGN_VERSION}")

    def verify(self, ref, allow_absent=False):
        for attempt in range(1, ATTEMPTS + 1):
            result = self.call([
                "verify", "--certificate-identity", IDENTITY,
                "--certificate-oidc-issuer", ISSUER, "--output", "json", ref,
            ], self.anonymous, f"verify {ref}")
            if result is not None and result.returncode == 0:
                payloads = parse_json(result.stdout, "cosign verification")
                require(isinstance(payloads, list) and payloads
                        and all(isinstance(payload, dict)
                                and isinstance(payload.get("critical"), dict)
                                and isinstance(payload["critical"].get("image"), dict)
                                and payload["critical"]["image"].get("docker-manifest-digest")
                                == ref.split("@")[1] for payload in payloads),
                        "Cosign verification returned an unexpected digest or output shape")
                return True
            # v2.5.2 has a dedicated no-signatures exit code. Any other error
            # (including network/auth/trust failures) is NOT absence.
            if (allow_absent and result is not None and result.returncode == 10
                    and not result.stdout.strip() and result.stderr.strip().splitlines() == [
                        b"Error: no signatures found",
                        b"error during command execution: no signatures found",
                    ]):
                return False
            outcome = "timeout" if result is None else f"exit {result.returncode}"
            print(f"verify {ref}: {outcome} (attempt {attempt}/{ATTEMPTS})", file=sys.stderr)
            self.commands.wait(attempt)
        raise RepairError(f"Cosign verification failed after {ATTEMPTS} attempts")

    def login(self):
        token = os.environ.get("REPAIR_GITHUB_TOKEN", "")
        actor = os.environ.get("GITHUB_ACTOR", "")
        require(token and actor, "Execute requires the workflow package token and actor")
        for variable in ("ACTIONS_ID_TOKEN_REQUEST_URL", "ACTIONS_ID_TOKEN_REQUEST_TOKEN"):
            require(os.environ.get(variable), "Execute requires ambient GitHub Actions OIDC")
            self.signing[variable] = os.environ[variable]
        self.signing["GITHUB_ACTIONS"] = "true"
        self.retry(["login", "ghcr.io", "--username", actor, "--password-stdin"],
                   "GHCR login", (token + "\n").encode())

    def retry(self, arguments, label, stdin=None):
        for attempt in range(1, ATTEMPTS + 1):
            result = self.call(arguments, self.signing, label, stdin)
            if result is not None and result.returncode == 0:
                return
            outcome = "timeout" if result is None else f"exit {result.returncode}"
            print(f"{label}: {outcome} (attempt {attempt}/{ATTEMPTS})", file=sys.stderr)
            self.commands.wait(attempt)
        raise RepairError(f"{label}: failed after {ATTEMPTS} attempts")

    def sign(self, ref):
        self.retry(["sign", "--yes", ref], f"sign {ref}")


def repair(batch, mode, directory, report, commands):
    registry = PublicRegistry(directory, commands)
    cosign = Cosign(directory, commands)
    checkout = commands.run(["git", "rev-parse", "HEAD"], registry.env, 10, "trusted checkout")
    require(checkout is not None and checkout.returncode == 0
            and checkout.stdout.strip() == os.environ["GITHUB_SHA"].encode(),
            "Repair checkout does not match the trusted main workflow commit")
    cosign.check_version()
    registry.verify_provenance(batch["provenance"])
    report["provenance_validated"] = True
    for artifact, row in zip(batch["artifacts"], report["artifacts"]):
        try:
            registry.validate_artifact(artifact)
            row["metadata_validated"] = True
            if cosign.verify(row["reference"], allow_absent=True):
                row.update(status="already-valid", verification="verified")
            else:
                registry.require_absent(artifact)
                row.update(status="would-sign", verification="absent")
        except RepairError as error:
            row.update(status="failed", error=str(error))
            print(f"{row['reference']}: {error}", file=sys.stderr)
    require(not any(row["status"] == "failed" for row in report["artifacts"]),
            "Inventory preflight failed; no signing or login attempted")
    report["inventory_validated"] = True
    if mode == "plan":
        report["status"] = "plan-ready"
        return
    pending = [(artifact, row) for artifact, row in zip(batch["artifacts"], report["artifacts"])
               if row["status"] == "would-sign"]
    if pending:
        try:
            cosign.login()
        except RepairError as error:
            for _, row in pending:
                row.update(status="failed", error=str(error))
            raise
    for artifact, row in pending:
        try:
            # Recheck after preflight, including all supported signature locations.
            # Another publisher may have signed since the inventory was checked.
            if cosign.verify(row["reference"], allow_absent=True):
                row.update(status="already-valid", verification="verified")
                continue
            registry.require_absent(artifact)
            cosign.sign(row["reference"])
            cosign.verify(row["reference"])
            row.update(status="repaired", verification="verified")
        except RepairError as error:
            row.update(status="failed", verification="failed", error=str(error))
            raise
    require(all(row["verification"] == "verified" for row in report["artifacts"]),
            "Not every selected artifact passed final verification")
    report["status"] = "verified"


def output(name, value):
    require(os.environ.get("GITHUB_OUTPUT"), "GITHUB_OUTPUT is required")
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as handle:
        handle.write(f"{name}={value}\n")


def write_report(report):
    report["all_verified"] = all(
        row["verification"] == "verified" for row in report["artifacts"]
    )
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", prefix="plugin-signature-repair-", suffix=".json",
        dir=os.environ["RUNNER_TEMP"], delete=False,
    ) as handle:
        json.dump(report, handle, indent=2)
        handle.write("\n")
        output("report", handle.name)
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as summary:
        summary.write(f"## Plugin signature repair: {report['mode']} / {report['status']}\n\n")
        summary.write(f"Batch: `{report['batch']}`. Source: `{report['provenance']['source_commit']}`.\n\n")
        if report["mode"] == "plan":
            summary.write("Read-only plan; no login or signing was performed.\n\n")
        summary.write("| Immutable artifact | Status | Metadata/checksum | Verification |\n")
        summary.write("| --- | --- | --- | --- |\n")
        for row in report["artifacts"]:
            checked = "validated" if row["metadata_validated"] else "not validated"
            summary.write(
                f"| `{row['reference']}` | {row['status']} | {checked} | {row['verification']} |\n"
            )
        summary.write("\nThe JSON report contains the pinned metadata, binary SHA256/size, "
                      "provenance, command outcomes and any failure. Only signature attachments "
                      "may be written by execute; no binaries or release tags are published.\n")
        if "error" in report:
            summary.write(f"\n**Failure:** {report['error']}\n")
    print(f"Signature repair {report['mode']}: {report['status']}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("select", "plan", "execute"))
    arguments = parser.parse_args()
    mode, name, batch = selection()
    if arguments.command == "select":
        output("mode", mode)
        return 0
    require(arguments.command == mode and mode in ("plan", "execute"),
            "Helper command does not match the explicit repair selection")
    report = {
        "batch": name, "mode": mode, "status": "failed",
        "identity": IDENTITY, "issuer": ISSUER, "cosign_version": COSIGN_VERSION,
        "provenance": batch["provenance"], "provenance_validated": False,
        "inventory_validated": False,
        "artifacts": [{
            "reference": reference(artifact), "platform": artifact["platform"],
            "release_tag": artifact["release_tag"], "binary": artifact["binary"],
            "metadata": artifact["metadata"], "metadata_validated": False,
            "status": "blocked", "verification": "not-checked",
        } for artifact in batch["artifacts"]],
    }
    commands = Commands()
    try:
        with tempfile.TemporaryDirectory(prefix="plugin-signature-repair-") as temporary:
            repair(batch, mode, Path(temporary), report, commands)
    except (RepairError, OSError) as error:
        message = str(error) if isinstance(error, RepairError) else "Repair I/O failed"
        report["error"] = message
        for row in report["artifacts"]:
            if row["status"] == "would-sign":
                row["status"] = "blocked"
        print(f"::error::{message}", file=sys.stderr)
    finally:
        report["commands"] = commands.attempts
        write_report(report)
    return 0 if report["status"] in ("plan-ready", "verified") else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RepairError, OSError) as error:
        # OSError messages can include credential-bearing paths; don't print them.
        message = str(error) if isinstance(error, RepairError) else "Repair I/O failed"
        print(f"::error::{message}", file=sys.stderr)
        sys.exit(1)
