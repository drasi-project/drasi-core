#!/usr/bin/env python3
"""Offline integration tests: stdlib unittest, Ruby's YAML parser, no registry writes."""

import ast
from contextlib import redirect_stderr, redirect_stdout
import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import runpy
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit


ROOT = Path(__file__).resolve().parents[2]
HELPER = ROOT / ".github/scripts/repair-plugin-signatures.py"
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("repair", HELPER)
repair = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(repair)
BATCH = "release-2026-09-25-darwin-arm64"
REVIEWED = json.loads(repair.INVENTORY.read_text())
SHA = "a" * 40
SECRET = "TEST-SECRET-MUST-NOT-APPEAR"


def encoded(value):
    return json.dumps(value, separators=(",", ":")).encode()


def digest(data):
    return "sha256:" + hashlib.sha256(data).hexdigest()


def workflow(path):
    result = subprocess.run(
        ["ruby", "-rjson", "-ryaml", "-e", "print JSON.generate(YAML.load(STDIN.read))"],
        input=path.read_text(), text=True, capture_output=True, check=True, timeout=10,
    )
    document = json.loads(result.stdout)
    # Ruby's YAML 1.1 reader treats the GitHub key "on" as a boolean.
    document["on"] = document.pop("true", document.get("on"))
    return document


def expression(source, context):
    """Evaluate the workflow's boolean gates with a parser, never eval/shell."""
    source = source.removeprefix("${{").removesuffix("}}").strip()
    tokens = re.findall(r"'[^']*'|&&|\|\||==|!=|[!()]|[A-Za-z_][A-Za-z0-9_.-]*", source)
    if re.sub(r"\s+", "", "".join(tokens)) != re.sub(r"\s+", "", source):
        raise AssertionError("Unsupported workflow expression")
    translated = []
    for token in tokens:
        if token in ("&&", "||", "!"):
            translated.append({"&&": "and", "||": "or", "!": "not"}[token])
        elif token in ("==", "!=", "(", ")") or token.startswith("'"):
            translated.append(token)
        else:
            value = context
            for name in token.split("."):
                value = value.get(name, "") if isinstance(value, dict) else ""
            translated.append(repr(value))
    tree = ast.parse(" ".join(translated), mode="eval")

    def visit(node):
        if isinstance(node, ast.Expression):
            return visit(node.body)
        if isinstance(node, ast.Constant):
            return node.value
        if isinstance(node, ast.UnaryOp) and isinstance(node.op, ast.Not):
            return not visit(node.operand)
        if isinstance(node, ast.BoolOp):
            values = [bool(visit(value)) for value in node.values]
            return all(values) if isinstance(node.op, ast.And) else any(values)
        if isinstance(node, ast.Compare) and len(node.ops) == 1:
            left, right = visit(node.left), visit(node.comparators[0])
            if isinstance(node.ops[0], ast.Eq):
                return left == right
            if isinstance(node.ops[0], ast.NotEq):
                return left != right
        raise AssertionError("Unsupported parsed workflow expression")

    return bool(visit(tree))


class Fixture:
    """Simulate curl/cosign/git at the process boundary; unknown commands fail."""

    def __init__(self, directory):
        self.directory = Path(directory)
        self.inventory = copy.deepcopy(REVIEWED)
        self.batch = self.inventory["batches"][BATCH]
        self.artifacts = self.batch["artifacts"]
        self.source = b'pub const FFI_SDK_VERSION: &str = "0.14.0";\n'
        self.batch["provenance"]["ffi_source_sha256"] = hashlib.sha256(self.source).hexdigest()
        self.blobs = {}
        self.manifests = {}
        for artifact in self.artifacts:
            kind = artifact["metadata"]["fields"]["kind"]
            self.blobs[kind] = {
                "binary": struct.pack("<8I", 0xFEEDFACF, 0x100000C, 0, 6, 1, 24, 0, 0)
                + struct.pack("<6I", 0x32, 24, 1, 0xB0000, 0xF0000, 0)
                + kind.encode(),
                "metadata": encoded(artifact["metadata"]["fields"]),
                "config": b"{}",
            }
        self.inventory_path = self.directory / ".github/plugin-signature-repairs.json"
        self.inventory_path.parent.mkdir(parents=True)
        self.helper = self.directory / ".github/scripts/repair-plugin-signatures.py"
        self.helper.parent.mkdir()
        shutil.copyfile(HELPER, self.helper)
        self.refresh()
        provenance = self.batch["provenance"]
        self.run_record = {
            "id": provenance["run_id"], "head_sha": provenance["source_commit"],
            "head_branch": "main", "path": provenance["run_path"],
            "event": provenance["run_event"], "status": "completed",
            "repository": {"full_name": repair.REPOSITORY},
        }
        self.job_record = {
            "id": provenance["job_id"], "run_id": provenance["run_id"],
            "name": provenance["job_name"], "status": "completed",
        }
        self.calls = []
        self.signed = set()
        self.verify_sequences = {}
        self.sign_sequences = {}
        self.http_sequences = {}
        self.http_overrides = {}
        self.post_verify_failure = False
        self.cosign_version = repair.COSIGN_VERSION
        self.login_failure = False
        self.checkout_sha = SHA
        self.inputs = {**repair.PUBLISH_DEFAULTS, "repair_batch": BATCH, "repair_action": "plan"}
        self.env = {
            "PATH": os.environ["PATH"], "GITHUB_REPOSITORY": repair.REPOSITORY,
            "GITHUB_REF": "refs/heads/main", "GITHUB_EVENT_NAME": "workflow_dispatch",
            "GITHUB_WORKFLOW_REF": repair.WORKFLOW_REF, "GITHUB_SHA": SHA,
            "GITHUB_WORKFLOW_SHA": SHA, "GITHUB_ACTIONS": "true", "GITHUB_ACTOR": "operator",
            "REPAIR_GITHUB_TOKEN": SECRET, "ACTIONS_ID_TOKEN_REQUEST_TOKEN": SECRET,
            "ACTIONS_ID_TOKEN_REQUEST_URL": "https://oidc.actions.githubusercontent.com/test",
            "COSIGN_KEY": SECRET, "COSIGN_REPOSITORY": "evil.example/override",
            "SIGSTORE_ROOT_FILE": SECRET, "HTTP_PROXY": SECRET, "DOCKER_AUTH_CONFIG": SECRET,
            "GITHUB_OUTPUT": str(self.directory / "outputs"),
            "GITHUB_STEP_SUMMARY": str(self.directory / "summary"),
            "RUNNER_TEMP": str(self.directory),
        }

    def refresh(self):
        for artifact in self.artifacts:
            kind = artifact["metadata"]["fields"]["kind"]
            for layer in ("binary", "metadata"):
                data = self.blobs[kind][layer]
                artifact[layer].update(digest=digest(data), size=len(data))
            fields = artifact["metadata"]["fields"]
            self.manifests[kind] = encoded({
                "schemaVersion": 2,
                "annotations": {key: fields[field] for key, field in repair.ANNOTATIONS.items()},
                "config": repair.CONFIG,
                "layers": [
                    {**artifact["binary"], "mediaType": repair.MEDIA_PREFIX + "binary"},
                    {key: artifact["metadata"][key] for key in ("digest", "size")}
                    | {"mediaType": repair.MEDIA_PREFIX + "metadata"},
                ],
            })
            artifact["manifest_digest"] = digest(self.manifests[kind])
        self.save()

    def save(self):
        self.inventory_path.write_bytes(encoded(self.inventory))

    def url(self, index, path):
        return f"https://ghcr.io/v2/{self.artifacts[index]['repository']}/{path}"

    def sig_url(self, index, location):
        value = self.artifacts[index]["manifest_digest"]
        tag = "sha256-" + value[7:]
        path = {
            "legacy": "manifests/" + tag + ".sig", "bundle": "manifests/" + tag,
            "referrers": "referrers/" + value,
        }[location]
        return self.url(index, path)

    @staticmethod
    def response(argv, code=0, data=b"", error=b""):
        return subprocess.CompletedProcess(argv, code, data, error)

    def http(self, url):
        if url in self.http_overrides:
            return self.http_overrides[url]
        provenance = self.batch["provenance"]
        base = f"https://api.github.com/repos/{repair.REPOSITORY}/actions"
        if url == f"{base}/runs/{provenance['run_id']}":
            return 200, "application/json", encoded(self.run_record)
        if url == f"{base}/jobs/{provenance['job_id']}":
            return 200, "application/json", encoded(self.job_record)
        if url == (
            f"https://raw.githubusercontent.com/{repair.REPOSITORY}/{provenance['source_commit']}"
            "/components/plugin-sdk/src/ffi/metadata.rs"
        ):
            return 200, "text/plain", self.source
        parsed = urlsplit(url)
        if parsed.netloc == "ghcr.io" and parsed.path == "/token":
            query = parse_qs(parsed.query)
            assert query["service"] == ["ghcr.io"]
            assert query["scope"] in [
                [f"repository:{artifact['repository']}:pull"] for artifact in self.artifacts
            ]
            return 200, "application/json", encoded({"token": "anonymous-read-only-token"})
        for index, artifact in enumerate(self.artifacts):
            kind = artifact["metadata"]["fields"]["kind"]
            if url == self.url(index, "manifests/" + artifact["manifest_digest"]):
                return 200, repair.MANIFEST_TYPE, self.manifests[kind]
            for layer in ("binary", "metadata", "config"):
                descriptor = repair.CONFIG if layer == "config" else artifact[layer]
                if url == self.url(index, "blobs/" + descriptor["digest"]):
                    return 200, "application/octet-stream", self.blobs[kind][layer]
            for location in ("legacy", "bundle", "referrers"):
                if url == self.sig_url(index, location):
                    if kind in self.signed and location == "legacy":
                        return 200, repair.MANIFEST_TYPE, b'{"existing":"signature"}'
                    return 404, "application/json", encoded({
                        "errors": [{"code": "MANIFEST_UNKNOWN", "message": "manifest unknown"}],
                    })
        raise AssertionError(f"Unexpected URL (real HTTP is forbidden): {url}")

    def run(self, argv, *, input, stdout, stderr, env, timeout, check):
        self.calls.append({"argv": argv, "env": env.copy(), "timeout": timeout, "stdin": input})
        assert timeout > 0 and not check
        assert stdout == subprocess.PIPE and stderr == subprocess.PIPE
        assert SECRET not in json.dumps(argv)
        if argv == ["git", "rev-parse", "HEAD"]:
            return self.response(argv, data=(self.checkout_sha + "\n").encode())
        if argv[0] == "curl":
            assert argv[:6] == [
                "curl", "--disable", "--silent", "--show-error", "--location", "--proto",
            ]
            assert argv[argv.index("--proto") + 1] == "=https"
            assert argv[argv.index("--proto-redir") + 1] == "=https"
            assert argv[argv.index("--config") + 1] == "-"
            assert timeout == repair.HTTP_TIMEOUT + 5
            assert SECRET not in json.dumps(env) and SECRET.encode() not in input
            url = argv[-1]
            sequence = self.http_sequences.get(url, [])
            code = sequence.pop(0) if sequence else None
            if code == "timeout":
                raise subprocess.TimeoutExpired(argv, timeout, stderr=SECRET.encode())
            if code == "transport":
                return self.response(argv, 7, error=SECRET.encode())
            status, content_type, body = self.http(url)
            if isinstance(code, int):
                status = code
            Path(argv[argv.index("--output") + 1]).write_bytes(body)
            return self.response(argv, data=f"{status}\n{content_type}".encode())
        assert argv[:3] == ["cosign", "--timeout", f"{repair.COSIGN_TIMEOUT}s"]
        operation = argv[3]
        assert timeout == repair.COSIGN_TIMEOUT + 5
        assert not ({"COSIGN_KEY", "COSIGN_REPOSITORY", "SIGSTORE_ROOT_FILE",
                     "HTTP_PROXY", "DOCKER_AUTH_CONFIG", "REPAIR_GITHUB_TOKEN"} & set(env))
        if operation in ("version", "verify"):
            assert SECRET not in json.dumps(env)
        if operation == "version":
            assert argv[4:] == ["--json"]
            return self.response(argv, data=encoded({"gitVersion": self.cosign_version}))
        if operation == "login":
            assert argv[4:] == ["ghcr.io", "--username", "operator", "--password-stdin"]
            assert input == (SECRET + "\n").encode()
            config = Path(env["DOCKER_CONFIG"]) / "config.json"
            config.parent.mkdir(exist_ok=True)
            config.write_text(SECRET)
            return self.response(argv, int(self.login_failure), error=SECRET.encode())
        ref = argv[-1]
        artifact = next(item for item in self.artifacts if repair.reference(item) == ref)
        kind = artifact["metadata"]["fields"]["kind"]
        if operation == "sign":
            assert argv[4:] == ["--yes", ref]
            assert env["ACTIONS_ID_TOKEN_REQUEST_TOKEN"] == SECRET
            assert env["GITHUB_ACTIONS"] == "true"
            sequence = self.sign_sequences.get(kind, [])
            outcome = sequence.pop(0) if sequence else "valid"
            if outcome == "timeout":
                raise subprocess.TimeoutExpired(argv, timeout, stderr=SECRET.encode())
            if outcome == "failure":
                return self.response(argv, 1, error=SECRET.encode())
            self.signed.add(kind)
            return self.response(argv)
        assert operation == "verify", "Only version/login/verify/sign may invoke cosign"
        assert argv[4:] == [
            "--certificate-identity", repair.IDENTITY, "--certificate-oidc-issuer",
            repair.ISSUER, "--output", "json", ref,
        ]
        sequence = self.verify_sequences.get(kind, [])
        outcome = sequence.pop(0) if sequence else (
            "network" if kind in self.signed and self.post_verify_failure else
            "valid" if kind in self.signed else "unsigned"
        )
        if outcome == "timeout":
            raise subprocess.TimeoutExpired(argv, timeout, stderr=SECRET.encode())
        if outcome in ("network", "wrong-signer"):
            return self.response(argv, 1, error=(outcome + " " + SECRET).encode())
        if outcome == "unrecognized-absence":
            return self.response(argv, 10, error=("network error: " + SECRET).encode())
        if outcome == "unsigned":
            return self.response(
                argv, 10, error=b"Error: no signatures found\n"
                b"error during command execution: no signatures found\n",
            )
        if outcome == "malformed":
            return self.response(argv, data=b"not-json " + SECRET.encode())
        if outcome == "empty":
            return self.response(argv, data=b"[]")
        verified_digest = "sha256:" + "0" * 64 if outcome == "wrong-digest" else ref.split("@")[1]
        return self.response(argv, data=encoded([{
            "critical": {"image": {"docker-manifest-digest": verified_digest}},
        }]))

    def invoke(self, command="plan"):
        self.env["WORKFLOW_INPUTS"] = json.dumps(self.inputs)
        output = Path(self.env["GITHUB_OUTPUT"])
        output.write_text("")
        with patch.dict(os.environ, self.env, clear=True), patch.object(
            repair, "INVENTORY", self.inventory_path
        ), patch.object(repair.subprocess, "run", side_effect=self.run), patch.object(
            repair.time, "sleep"
        ) as sleep, patch.object(sys, "argv", [str(HELPER), command]), redirect_stdout(
            io.StringIO()
        ) as stdout, redirect_stderr(io.StringIO()) as stderr:
            try:
                code = repair.main()
            except repair.RepairError as error:
                code = 1
                print(error, file=sys.stderr)
        outputs = dict(line.split("=", 1) for line in output.read_text().splitlines())
        report = json.loads(Path(outputs["report"]).read_text()) if "report" in outputs else None
        return code, report, stdout.getvalue() + stderr.getvalue(), outputs, sleep.call_count

    def operations(self, operation):
        return [call for call in self.calls
                if call["argv"][0] == "cosign" and call["argv"][3] == operation]


class RepairTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.fixture = Fixture(self.temporary.name)

    def assert_no_writes(self):
        self.assertEqual([], self.fixture.operations("sign"))
        self.assertEqual([], self.fixture.operations("login"))

    def assert_failure(self, command="plan", expected=None):
        code, report, logs, _, _ = self.fixture.invoke(command)
        self.assertEqual(1, code, logs)
        if report:
            self.assertEqual("failed", report["status"])
        if expected:
            self.assertIn(expected, logs + json.dumps(report))
        self.assertNotIn(SECRET, logs + json.dumps(report))
        return report

    def test_reviewed_inventory_is_the_two_exact_incident_artifacts(self):
        batch = repair.load_batch(BATCH)
        self.assertEqual([
            "sha256:60c70c051c53c423caa9e60d55f09d01fbdea853ff202450412bdc06cf262ad5",
            "sha256:87327fac1978a58f0aace2a5059d4e169c2245f0cd2cde62c33e47d18966677c",
        ], [item["manifest_digest"] for item in batch["artifacts"]])
        self.assertEqual([4674160, 6292944], [item["binary"]["size"] for item in batch["artifacts"]])
        self.assertEqual(["0.3.8", "0.1.6"], [
            item["metadata"]["fields"]["version"] for item in batch["artifacts"]
        ])
        self.assertEqual([
            "sha256:75c6e1433afdc573b02091fd9b2c768fe0f1b227ec8221a9ab02ac612348b733",
            "sha256:d12d51e73ddb2d36a772c5038818cb0802c41c43998815dc8328f3479ed91dda",
        ], [item["binary"]["digest"] for item in batch["artifacts"]])
        self.assertEqual("0.14.0", batch["provenance"]["ffi_sdk_version"])
        self.assertEqual(
            "5a58bb191eed01ea896dd7a7b38f9ef53fd82b7b5edd717b05c54f59888d01f9",
            batch["provenance"]["ffi_source_sha256"],
        )

    def test_plan_is_read_only_and_explicitly_not_verified(self):
        code, report, logs, outputs, _ = self.fixture.invoke()
        self.assertEqual(0, code, logs)
        self.assertEqual("plan-ready", report["status"])
        self.assertFalse(report["all_verified"])
        self.assertTrue(report["inventory_validated"])
        self.assertTrue(report["provenance_validated"])
        self.assertEqual(["would-sign", "would-sign"], [r["status"] for r in report["artifacts"]])
        self.assert_no_writes()
        self.assertTrue(Path(outputs["report"]).is_file())
        self.assertEqual(0o600, Path(outputs["report"]).stat().st_mode & 0o777)
        self.assertNotIn(SECRET, logs + json.dumps(report))

    def test_execute_validates_whole_inventory_before_login_or_sign_and_is_idempotent(self):
        self.fixture.inputs["repair_action"] = "execute"
        code, report, logs, _, _ = self.fixture.invoke("execute")
        self.assertEqual(0, code, logs)
        self.assertTrue(report["all_verified"])
        self.assertEqual(["repaired", "repaired"], [r["status"] for r in report["artifacts"]])
        self.assertEqual(2, len(self.fixture.operations("sign")))
        first_write = next(i for i, c in enumerate(self.fixture.calls)
                           if c["argv"][0] == "cosign" and c["argv"][3] == "login")
        before_write = [c["argv"][-1] for c in self.fixture.calls[:first_write]]
        for index, artifact in enumerate(self.fixture.artifacts):
            for layer in ("binary", "metadata"):
                self.assertIn(self.fixture.url(index, "blobs/" + artifact[layer]["digest"]),
                              before_write)
            for location in ("legacy", "bundle", "referrers"):
                self.assertIn(self.fixture.sig_url(index, location), before_write)
        self.fixture.calls.clear()
        code, report, logs, _, _ = self.fixture.invoke("execute")
        self.assertEqual(0, code, logs)
        self.assertEqual(["already-valid", "already-valid"], [
            r["status"] for r in report["artifacts"]
        ])
        self.assert_no_writes()

    def test_already_valid_and_mixed_inventory(self):
        self.fixture.inputs["repair_action"] = "execute"
        self.fixture.signed.add("sse")
        code, report, logs, _, _ = self.fixture.invoke("execute")
        self.assertEqual(0, code, logs)
        self.assertEqual(["already-valid", "repaired"], [r["status"] for r in report["artifacts"]])
        self.assertEqual(1, len(self.fixture.operations("sign")))

    def test_wrong_context_never_reaches_external_commands(self):
        for variable, value in (
            ("GITHUB_REPOSITORY", "attacker/drasi-core"), ("GITHUB_REF", "refs/heads/topic"),
            ("GITHUB_REF", "refs/tags/v1"), ("GITHUB_EVENT_NAME", "push"),
            ("GITHUB_EVENT_NAME", "pull_request"), ("GITHUB_EVENT_NAME", "workflow_call"),
            ("GITHUB_WORKFLOW_REF", "attacker/workflow.yml@refs/heads/main"),
            ("GITHUB_WORKFLOW_SHA", "b" * 40), ("GITHUB_SHA", "main"),
            ("GITHUB_ACTIONS", "false"),
        ):
            for mode in ("plan", "execute"):
                with self.subTest(variable=variable, value=value, mode=mode):
                    self.fixture.inputs["repair_action"] = mode
                    with patch.dict(self.fixture.env, {variable: value}):
                        self.assert_failure(mode, "Repair requires")
                    self.assertEqual([], self.fixture.calls)

    def test_publication_overrides_and_arbitrary_targets_are_rejected_in_repair(self):
        for field, value in (
            ("ref", "refs/pull/1/head"), ("ref", "$(touch /tmp/not-executed)"),
            ("registry", "attacker.example/repo"), ("tag", "latest"),
            ("pre_release", "dev"), ("sign", False), ("dry_run", True),
            ("skip_visibility", True), ("sign", "true"),
            ("digest", "sha256:" + "1" * 64),
        ):
            with self.subTest(field=field):
                with patch.dict(self.fixture.inputs, {field: value}):
                    self.assert_failure()
                self.assertEqual([], self.fixture.calls)

    def test_unknown_empty_and_inconsistent_selections_do_not_fall_back_to_publish(self):
        for batch, action in (
            ("", "plan"), ("../other", "execute"), ("none", "execute"),
            ("sha256:" + "1" * 64, "plan"), ("evil.example/x@sha256:" + "1" * 64, "execute"),
            (BATCH, ""), (BATCH, "invalid"), (BATCH, "$(sign)"), (None, "execute"),
        ):
            with self.subTest(batch=batch, action=action):
                with patch.dict(self.fixture.inputs, {"repair_batch": batch, "repair_action": action}):
                    self.assert_failure("select")
                self.assertEqual([], self.fixture.calls)
        del self.fixture.inputs["repair_action"]
        self.assert_failure("select", "Incomplete")

    def test_helper_command_must_match_explicit_selection(self):
        self.assert_failure("execute", "does not match")
        self.fixture.inputs["repair_action"] = "execute"
        self.assert_failure("plan", "does not match")
        self.assert_no_writes()

    def test_normal_dispatch_and_reusable_call_preserve_existing_inputs(self):
        for inputs in (
            {**repair.PUBLISH_DEFAULTS, "repair_batch": "none", "repair_action": "plan"},
            repair.PUBLISH_DEFAULTS,
            {**repair.PUBLISH_DEFAULTS, "ref": "topic", "sign": False, "dry_run": True},
            {**repair.PUBLISH_DEFAULTS, "tag": "nightly", "skip_visibility": True},
        ):
            with self.subTest(inputs=inputs):
                self.fixture.inputs = inputs
                code, _, logs, outputs, _ = self.fixture.invoke("select")
                self.assertEqual(0, code, logs)
                self.assertEqual("publish", outputs["mode"])
                self.assertEqual([], self.fixture.calls)

    def test_bad_inventory_is_rejected_before_any_network_request(self):
        for field, value in (
            ("manifest_digest", "latest"), ("manifest_digest", "sha256:" + "Z" * 64),
            ("repository", "evil.example/reaction/sse"), ("platform", "linux/arm64"),
            ("release_tag", "latest"),
        ):
            with self.subTest(field=field):
                with patch.dict(self.fixture.artifacts[0], {field: value}):
                    self.fixture.save()
                    self.assert_failure("select")
                self.assertEqual([], self.fixture.calls)
        with patch.dict(self.fixture.artifacts[0]["binary"], {"size": -1}):
            self.fixture.save()
            self.assert_failure("select", "digest/size")
        self.fixture.artifacts.append(copy.deepcopy(self.fixture.artifacts[0]))
        self.fixture.save()
        self.assert_failure("select", "Duplicate")

    def test_wrong_checkout_and_unpinned_cosign_fail_closed(self):
        self.fixture.checkout_sha = "b" * 40
        self.assert_failure(expected="checkout")
        self.fixture.checkout_sha = SHA
        self.fixture.cosign_version = "v999.0.0"
        self.assert_failure(expected="pinned cosign")
        self.assert_no_writes()

    def test_original_release_provenance_mismatches_fail_before_signing(self):
        for record, field, value in (
            (self.fixture.run_record, "head_sha", "b" * 40),
            (self.fixture.run_record, "head_branch", "topic"),
            (self.fixture.run_record, "event", "pull_request"),
            (self.fixture.run_record, "path", ".github/workflows/other.yml"),
            (self.fixture.run_record, "repository", {"full_name": "attacker/repo"}),
            (self.fixture.job_record, "run_id", 1),
            (self.fixture.job_record, "name", "untrusted job"),
        ):
            with self.subTest(field=field):
                with patch.dict(record, {field: value}):
                    self.assert_failure(expected="provenance mismatch")
                self.assert_no_writes()
        self.fixture.source = self.fixture.source.replace(b"0.14.0", b"0.99.0")
        self.assert_failure(expected="FFI ABI mismatch")
        self.assert_no_writes()

    def test_missing_second_artifact_aborts_entire_preflight_without_login(self):
        self.fixture.inputs["repair_action"] = "execute"
        artifact = self.fixture.artifacts[1]
        url = self.fixture.url(1, "manifests/" + artifact["manifest_digest"])
        self.fixture.http_overrides[url] = (404, "application/json", b"{}")
        report = self.assert_failure("execute", "no signing or login attempted")
        self.assertEqual(["blocked", "failed"], [row["status"] for row in report["artifacts"]])
        self.assert_no_writes()

    def test_manifest_digest_and_response_type_must_match(self):
        url = self.fixture.url(0, "manifests/" + self.fixture.artifacts[0]["manifest_digest"])
        for status, content_type, raw in (
            (200, repair.MANIFEST_TYPE, b"{}"),
            (200, "text/html", self.fixture.manifests["sse"]),
            (401, "application/json", b"{}"), (403, "application/json", b"{}"),
        ):
            with self.subTest(status=status, content_type=content_type):
                self.fixture.http_overrides[url] = status, content_type, raw
                self.assert_failure()
                self.assert_no_writes()

    def test_binary_checksum_and_size_are_validated(self):
        artifact = self.fixture.artifacts[1]
        url = self.fixture.url(1, "blobs/" + artifact["binary"]["digest"])
        original = self.fixture.blobs["rabbitmq"]["binary"]
        for body in (original[:-1], original + b"x", original[:-1] + b"X"):
            with self.subTest(size=len(body)):
                self.fixture.http_overrides[url] = 200, "application/octet-stream", body
                self.assert_failure(expected="plugin binary")
                self.assert_no_writes()

    def test_metadata_fields_and_binary_target_are_checked_beyond_digests(self):
        for field in ("kind", "type", "version", "sdk_version", "core_version",
                      "lib_version", "target_triple"):
            with self.subTest(field=field):
                fields = copy.deepcopy(self.fixture.artifacts[0]["metadata"]["fields"])
                fields[field] = "mismatch"
                self.fixture.blobs["sse"]["metadata"] = encoded(fields)
                self.fixture.refresh()
                self.assert_failure(expected="Plugin metadata mismatch")
                self.assert_no_writes()
        self.fixture.blobs["sse"]["metadata"] = encoded(
            self.fixture.artifacts[0]["metadata"]["fields"]
        )
        self.fixture.blobs["rabbitmq"]["binary"] = b"not-a-darwin-binary"
        self.fixture.refresh()
        self.assert_failure(expected="Mach-O dylib")
        self.assert_no_writes()

    def test_macho_arm64_is_not_sufficient_without_macos_build_platform(self):
        original = self.fixture.blobs["sse"]["binary"]
        for offset, value in ((40, 2), (36, 0), (20, 9999999), (16, 2)):
            with self.subTest(offset=offset, value=value):
                binary = bytearray(original)
                struct.pack_into("<I", binary, offset, value)
                self.fixture.blobs["sse"]["binary"] = bytes(binary)
                self.fixture.refresh()
                self.assert_failure(expected="Plugin binary")
                self.assert_no_writes()

    def test_config_and_metadata_blobs_must_match_their_pins(self):
        for layer in ("config", "metadata"):
            with self.subTest(layer=layer):
                descriptor = repair.CONFIG if layer == "config" else self.fixture.artifacts[0][layer]
                url = self.fixture.url(0, "blobs/" + descriptor["digest"])
                self.fixture.http_overrides[url] = (
                    200, "application/octet-stream",
                    b"x" * len(self.fixture.blobs["sse"][layer]),
                )
                self.assert_failure(expected=f"plugin {layer}: checksum mismatch")
                self.assert_no_writes()
                del self.fixture.http_overrides[url]

    def test_unexpected_manifest_annotation_is_not_a_source_authorization(self):
        manifest = json.loads(self.fixture.manifests["sse"])
        manifest["annotations"]["org.opencontainers.image.revision"] = "unreviewed"
        raw = encoded(manifest)
        self.fixture.manifests["sse"] = raw
        self.fixture.artifacts[0]["manifest_digest"] = digest(raw)
        self.fixture.save()
        self.assert_failure(expected="source annotations mismatch")
        self.assert_no_writes()

    def test_existing_signature_at_any_supported_location_is_not_absence(self):
        for location in ("legacy", "bundle", "referrers"):
            with self.subTest(location=location):
                url = self.fixture.sig_url(0, location)
                self.fixture.http_overrides[url] = 200, repair.INDEX_TYPE, b'{"malformed":true}'
                self.assert_failure()
                self.assert_no_writes()
                del self.fixture.http_overrides[url]

    def test_empty_oci_referrers_index_is_supported_absence(self):
        self.fixture.http_overrides[self.fixture.sig_url(0, "referrers")] = (
            200, repair.INDEX_TYPE,
            encoded({"schemaVersion": 2, "mediaType": repair.INDEX_TYPE, "manifests": []}),
        )
        code, _, logs, _, _ = self.fixture.invoke()
        self.assertEqual(0, code, logs)
        self.assert_no_writes()

    def test_auth_network_and_unexpected_404_are_not_absence(self):
        url = self.fixture.sig_url(0, "legacy")
        for status, content_type, body in (
            (401, "application/json", SECRET.encode()),
            (403, "application/json", SECRET.encode()),
            (404, "text/html", b"not found"),
            (404, "application/json", b"not-json " + SECRET.encode()),
            (404, "application/json", b'{"errors":[{"code":"UNAUTHORIZED"}]}'),
            (404, "application/json", b'{"errors":[{"code":"NAME_UNKNOWN"}]}'),
        ):
            with self.subTest(status=status, body=body):
                self.fixture.http_overrides[url] = status, content_type, body
                self.assert_failure()
                self.assert_no_writes()
        del self.fixture.http_overrides[url]
        self.fixture.http_sequences[url] = ["transport"] * repair.ATTEMPTS
        self.assert_failure(expected="request failed after 3 attempts")
        self.assert_no_writes()

    def test_verification_failures_are_not_unsigned_even_if_all_endpoints_would_be_404(self):
        for outcome in ("network", "wrong-signer", "timeout", "unrecognized-absence"):
            with self.subTest(outcome=outcome):
                self.fixture.verify_sequences["sse"] = [outcome] * repair.ATTEMPTS
                before = len(self.fixture.operations("verify"))
                self.assert_failure(expected="verification failed after 3 attempts")
                self.assertEqual(before + repair.ATTEMPTS + 1,
                                 len(self.fixture.operations("verify")))
                self.assert_no_writes()

    def test_success_exit_with_invalid_verification_output_fails(self):
        for outcome in ("malformed", "empty", "wrong-digest"):
            with self.subTest(outcome=outcome):
                self.fixture.verify_sequences["sse"] = [outcome]
                self.assert_failure(expected="verification")
                self.assert_no_writes()

    def test_transient_http_and_verification_failures_have_bounded_retries(self):
        url = self.fixture.url(0, "manifests/" + self.fixture.artifacts[0]["manifest_digest"])
        self.fixture.http_sequences[url] = [503, 429, 200]
        self.fixture.verify_sequences["sse"] = ["timeout", "network", "unsigned"]
        code, _, logs, _, sleeps = self.fixture.invoke()
        self.assertEqual(0, code, logs)
        self.assertEqual(4, sleeps)
        self.assertEqual(3, sum(c["argv"][-1] == url for c in self.fixture.calls))
        self.assert_no_writes()

    def test_http_timeout_exhaustion_is_bounded(self):
        url = self.fixture.url(0, "manifests/" + self.fixture.artifacts[0]["manifest_digest"])
        self.fixture.http_sequences[url] = ["timeout"] * repair.ATTEMPTS
        self.assert_failure(expected="request failed after 3 attempts")
        self.assertEqual(3, sum(c["argv"][-1] == url for c in self.fixture.calls))
        self.assert_no_writes()

    def test_sign_failure_stops_following_artifacts_and_fails_raw_result(self):
        self.fixture.inputs["repair_action"] = "execute"
        self.fixture.sign_sequences["sse"] = ["failure"] * repair.ATTEMPTS
        report = self.assert_failure("execute", "failed after 3 attempts")
        self.assertEqual(["failed", "blocked"], [row["status"] for row in report["artifacts"]])
        self.assertEqual(3, len(self.fixture.operations("sign")))
        self.assertFalse(report["all_verified"])

    def test_sign_timeout_and_transient_failure_retry_without_sleeping_in_tests(self):
        self.fixture.inputs["repair_action"] = "execute"
        self.fixture.sign_sequences["sse"] = ["timeout", "failure", "valid"]
        code, report, logs, _, sleeps = self.fixture.invoke("execute")
        self.assertEqual(0, code, logs)
        self.assertEqual("verified", report["status"])
        self.assertEqual(2, sleeps)
        self.assertEqual(4, len(self.fixture.operations("sign")))

    def test_post_sign_verification_is_required_and_failure_does_not_sign_next_artifact(self):
        self.fixture.inputs["repair_action"] = "execute"
        self.fixture.post_verify_failure = True
        report = self.assert_failure("execute", "verification failed after 3 attempts")
        self.assertEqual(1, len(self.fixture.operations("sign")))
        self.assertEqual("failed", report["artifacts"][0]["verification"])
        self.assertEqual("blocked", report["artifacts"][1]["status"])

    def test_concurrent_valid_signature_is_skipped_after_preflight(self):
        self.fixture.inputs["repair_action"] = "execute"
        self.fixture.verify_sequences["sse"] = ["unsigned", "valid"]
        code, report, logs, _, _ = self.fixture.invoke("execute")
        self.assertEqual(0, code, logs)
        self.assertEqual(["already-valid", "repaired"], [r["status"] for r in report["artifacts"]])
        self.assertEqual(1, len(self.fixture.operations("sign")))

    def test_signature_appearing_after_preflight_is_not_overwritten(self):
        self.fixture.inputs["repair_action"] = "execute"
        self.fixture.http_sequences[self.fixture.sig_url(0, "legacy")] = [404, 200]
        self.assert_failure("execute", "existing signatures")
        self.assertEqual([], self.fixture.operations("sign"))

    def test_execute_needs_oidc_and_package_auth_only_when_signing_is_needed(self):
        self.fixture.inputs["repair_action"] = "execute"
        with patch.dict(self.fixture.env, {"REPAIR_GITHUB_TOKEN": ""}):
            self.assert_failure("execute", "package token")
        with patch.dict(self.fixture.env, {"ACTIONS_ID_TOKEN_REQUEST_TOKEN": ""}):
            self.assert_failure("execute", "ambient GitHub Actions OIDC")
        self.assert_no_writes()
        self.fixture.signed.update(("sse", "rabbitmq"))
        with patch.dict(self.fixture.env, {"REPAIR_GITHUB_TOKEN": "",
                                           "ACTIONS_ID_TOKEN_REQUEST_TOKEN": ""}):
            code, report, logs, _, _ = self.fixture.invoke("execute")
        self.assertEqual(0, code, logs)
        self.assertTrue(report["all_verified"])
        self.assert_no_writes()

    def test_login_failure_is_bounded_and_secret_free(self):
        self.fixture.inputs["repair_action"] = "execute"
        self.fixture.login_failure = True
        self.assert_failure("execute", "GHCR login")
        self.assertEqual(3, len(self.fixture.operations("login")))
        self.assertEqual([], self.fixture.operations("sign"))

    def test_owned_temporary_credentials_and_http_files_are_removed_on_failure(self):
        self.fixture.inputs["repair_action"] = "execute"
        self.fixture.sign_sequences["sse"] = ["failure"] * repair.ATTEMPTS
        self.assert_failure("execute")
        for call in self.fixture.calls:
            if "HOME" in call["env"]:
                self.assertFalse(Path(call["env"]["HOME"]).exists())
            if call["argv"][0] == "curl":
                self.assertFalse(Path(call["argv"][call["argv"].index("--output") + 1]).exists())
        self.assertNotIn(SECRET, Path(self.fixture.env["GITHUB_STEP_SUMMARY"]).read_text())

    def test_owned_temporary_directories_are_removed_after_success(self):
        self.fixture.inputs["repair_action"] = "execute"
        code, report, logs, _, _ = self.fixture.invoke("execute")
        self.assertEqual(0, code, logs)
        self.assertNotIn(SECRET, logs + json.dumps(report))
        for call in self.fixture.calls:
            self.assertFalse(Path(call["env"]["HOME"]).exists())

    def test_raw_cli_exit_codes_and_arguments_with_offline_process_boundary(self):
        for mode, scenario, expected in (
            ("plan", "unsigned", 0), ("execute", "sign-failure", 1),
            ("execute", "post-verify-failure", 1), ("execute", "unsigned", 0),
            ("select", "invalid-json", 1), ("select", "duplicate-inputs", 1),
            ("select", "unknown-batch", 1), ("select", "wrong-ref", 1),
        ):
            with self.subTest(mode=mode, scenario=scenario):
                with tempfile.TemporaryDirectory() as directory:
                    result = subprocess.run([
                        sys.executable, str(Path(__file__)), "--fixture-cli",
                        directory, scenario, mode,
                    ], capture_output=True, text=True, timeout=20)
                    self.assertEqual(expected, result.returncode, result.stdout + result.stderr)
                    self.assertNotIn(SECRET, result.stdout + result.stderr)
        result = subprocess.run(
            [sys.executable, str(HELPER), "execute", "--digest", "arbitrary"],
            capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(2, result.returncode)
        self.assertIn("unrecognized arguments", result.stderr)


class WorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.document = workflow(ROOT / ".github/workflows/publish-plugins.yml")
        cls.jobs = cls.document["jobs"]

    def runnable(self, inputs, mode, github=None, selection_ok=True):
        github = {
            "repository": repair.REPOSITORY, "ref": "refs/heads/main",
            "event_name": "workflow_dispatch", "workflow_ref": repair.WORKFLOW_REF,
            **(github or {}),
        }
        results = {"select-mode": "success" if selection_ok else "failure"}
        pending = {k: v for k, v in self.jobs.items() if k != "select-mode"}
        while pending:
            progress = False
            for name, job in list(pending.items()):
                needs = job.get("needs", [])
                needs = [needs] if isinstance(needs, str) else needs
                if not all(need in results for need in needs):
                    continue
                context = {
                    "inputs": inputs, "github": github,
                    "needs": {"select-mode": {"outputs": {"mode": mode}}},
                }
                enabled = all(results[need] == "success" for need in needs)
                enabled = enabled and expression(job.get("if", "'' == ''"), context)
                results[name] = "success" if enabled else "skipped"
                del pending[name]
                progress = True
            self.assertTrue(progress, "Workflow needs graph must be acyclic")
        return {name for name, result in results.items() if result == "success"}

    def test_default_dispatch_and_workflow_call_contract(self):
        call = self.document["on"]["workflow_call"]["inputs"]
        dispatch = self.document["on"]["workflow_dispatch"]["inputs"]
        self.assertEqual(set(repair.PUBLISH_DEFAULTS), set(call))
        self.assertEqual(call, {key: dispatch[key] for key in call})
        self.assertEqual("none", dispatch["repair_batch"]["default"])
        self.assertEqual("plan", dispatch["repair_action"]["default"])
        self.assertEqual(["none", *REVIEWED["batches"]], dispatch["repair_batch"]["options"])
        self.assertEqual({"workflow_call", "workflow_dispatch"}, set(self.document["on"]))
        self.assertLessEqual(len(dispatch), 10)

    def test_selection_script_drives_actual_parsed_workflow_routes(self):
        normal = {
            "select-mode", "visibility-preflight", "list-plugin-packages",
            "build-and-publish", "verify-public-visibility",
        }
        for batch, action, expected in (
            ("none", "plan", normal),
            (BATCH, "plan", {"select-mode", "repair-plan"}),
            (BATCH, "execute", {"select-mode", "repair-execute"}),
            ("bad-batch", "execute", set()),
            ("none", "execute", set()),
        ):
            with self.subTest(batch=batch, action=action), tempfile.TemporaryDirectory() as directory:
                fixture = Fixture(directory)
                fixture.inputs.update(repair_batch=batch, repair_action=action)
                code, _, _, outputs, _ = fixture.invoke("select")
                self.assertEqual(expected, self.runnable(
                    fixture.inputs, outputs.get("mode", ""), selection_ok=code == 0,
                ))
        for field in ("dry_run", "skip_visibility"):
            inputs = {**repair.PUBLISH_DEFAULTS, field: True}
            self.assertEqual({"select-mode", "visibility-preflight", "build-and-publish"},
                             self.runnable(inputs, "publish"))

    def test_privileged_gate_rejects_untrusted_context_even_with_forged_selector_output(self):
        inputs = {**repair.PUBLISH_DEFAULTS, "repair_batch": BATCH, "repair_action": "execute"}
        for github in (
            {"repository": "attacker/drasi-core"}, {"ref": "refs/heads/topic"},
            {"event_name": "pull_request"}, {"event_name": "push"},
            {"event_name": "workflow_call"}, {"workflow_ref": "caller/workflow@refs/heads/main"},
        ):
            with self.subTest(github=github):
                self.assertNotIn("repair-execute", self.runnable(inputs, "execute", github))
        for override in ({"repair_batch": "arbitrary"}, {"repair_action": "plan"}):
            self.assertNotIn("repair-execute", self.runnable({**inputs, **override}, "execute"))

    def test_permissions_checkout_pins_and_batch_scoped_concurrency(self):
        self.assertEqual({"packages": "write", "contents": "read", "id-token": "write"},
                         self.document["permissions"])
        for name in ("select-mode", "repair-plan", "visibility-preflight",
                     "list-plugin-packages", "verify-public-visibility"):
            self.assertEqual({"contents": "read"}, self.jobs[name]["permissions"])
        self.assertNotIn("permissions", self.jobs["build-and-publish"])
        self.assertEqual({"contents": "read", "packages": "write", "id-token": "write"},
                         self.jobs["repair-execute"]["permissions"])
        self.assertEqual({
            "group": "plugin-signature-repair-${{ inputs.repair_batch }}",
            "cancel-in-progress": False,
        }, self.jobs["repair-execute"]["concurrency"])
        self.assertNotIn("concurrency", self.document)
        self.assertEqual("python3 .github/scripts/repair-plugin-signatures.py select",
                         self.jobs["select-mode"]["steps"][1]["run"])
        self.assertEqual("${{ steps.select.outputs.mode }}",
                         self.jobs["select-mode"]["outputs"]["mode"])
        for name in ("repair-plan", "repair-execute"):
            steps = self.jobs[name]["steps"]
            self.assertEqual({
                "repository": repair.REPOSITORY, "ref": "${{ github.sha }}",
                "persist-credentials": False,
            }, steps[0]["with"])
            self.assertEqual("v2.5.2", steps[1]["with"]["cosign-release"])
            self.assertRegex(steps[1]["uses"], r"sigstore/cosign-installer@[a-f0-9]{40}$")
            self.assertEqual(["python3", ".github/scripts/repair-plugin-signatures.py",
                              "plan" if name == "repair-plan" else "execute"],
                             shlex.split(steps[2]["run"]))
            self.assertEqual("${{ toJSON(inputs) }}", steps[2]["env"]["WORKFLOW_INPUTS"])
            self.assertNotIn("PACKAGES_ADMIN_TOKEN", json.dumps(self.jobs[name]))
            self.assertEqual("error", steps[3]["with"]["if-no-files-found"])
        self.assertEqual({"WORKFLOW_INPUTS": "${{ toJSON(inputs) }}"},
                         self.jobs["repair-plan"]["steps"][2]["env"])

    def test_focused_offline_suite_is_wired_to_read_only_ci(self):
        document = workflow(ROOT / ".github/workflows/test.yml")
        job = document["jobs"]["signature-repair"]
        self.assertEqual({"contents": "read"}, document["permissions"])
        self.assertEqual("python3 .github/scripts/repair-plugin-signatures.test.py",
                         job["steps"][1]["run"])
        self.assertNotIn("secrets", json.dumps(job))


def fixture_cli():
    fixture = Fixture(sys.argv[2])
    scenario, mode = sys.argv[3:]
    fixture.inputs["repair_action"] = "plan" if mode == "select" else mode
    if scenario == "sign-failure":
        fixture.sign_sequences["sse"] = ["failure"] * repair.ATTEMPTS
    elif scenario == "post-verify-failure":
        fixture.post_verify_failure = True
    elif scenario == "unknown-batch":
        fixture.inputs["repair_batch"] = "arbitrary"
    elif scenario == "wrong-ref":
        fixture.env["GITHUB_REF"] = "refs/heads/untrusted"
    else:
        assert scenario in ("unsigned", "invalid-json", "duplicate-inputs")
    fixture.env["WORKFLOW_INPUTS"] = json.dumps(fixture.inputs)
    if scenario == "invalid-json":
        fixture.env["WORKFLOW_INPUTS"] = SECRET
    elif scenario == "duplicate-inputs":
        fixture.env["WORKFLOW_INPUTS"] = '{"repair_batch":"none","repair_batch":"other"}'
    with patch.dict(os.environ, fixture.env, clear=True), patch(
        "subprocess.run", side_effect=fixture.run
    ), patch("time.sleep"), patch.object(sys, "argv", [str(fixture.helper), mode]):
        runpy.run_path(str(fixture.helper), run_name="__main__")


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--fixture-cli":
        fixture_cli()
    else:
        unittest.main()
