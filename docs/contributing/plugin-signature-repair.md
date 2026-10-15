# Repairing signatures on existing plugin releases

This is a **sign-only, manually dispatched recovery mechanism**, not another
publication pipeline. Merging the code does not repair any artifact or start a
repair run. The implementation must first be reviewed and merged into `main`.
An operator must then explicitly choose execution.

The mechanism is tracked in [#973](https://github.com/drasi-project/drasi-core/issues/973).
The original unsigned-artifact incident remains
[#970](https://github.com/drasi-project/drasi-core/issues/970) until actual repairs
are independently verified. Normal-publication failure propagation is addressed
separately by [#971](https://github.com/drasi-project/drasi-core/pull/971);
the broader reliability work in
[#972](https://github.com/drasi-project/drasi-core/issues/972) is not part of this repair.

## Reviewed inventory and provenance

The only initial batch is `release-2026-09-25-darwin-arm64`, in
[`.github/plugin-signature-repairs.json`](../../.github/plugin-signature-repairs.json).
It contains these immutable GHCR manifests:

| Plugin | Version / platform | Manifest SHA256 |
| --- | --- | --- |
| `reaction/sse` | `0.3.8` / `darwin/arm64` | `60c70c051c53c423caa9e60d55f09d01fbdea853ff202450412bdc06cf262ad5` |
| `reaction/rabbitmq` | `0.1.6` / `darwin/arm64` | `87327fac1978a58f0aace2a5059d4e169c2245f0cd2cde62c33e47d18966677c` |

On 2026-09-26 the original
[official job log](https://github.com/drasi-project/drasi-core/actions/runs/36166389551/job/108192160538)
was reviewed: it records both versioned tag uploads and these exact manifest
digests, followed by signing failures. The reusable workflow was
`publish-plugins.yml@refs/heads/main` at source commit
`22125bf1d66062533b832a166fe4a51079a23d6e`, called by `release-plz.yml`.
Anonymous GETs independently verified both manifests, all layer hashes/sizes,
metadata and arm64 Mach-O dylib headers. The `.sig` tag, bare bundle tag and OCI
referrers requests all returned registry `MANIFEST_UNKNOWN` 404 responses.

The checked-in binary SHA256 values are:

| Plugin | Binary SHA256 | Bytes |
| --- | --- | --- |
| SSE | `75c6e1433afdc573b02091fd9b2c768fe0f1b227ec8221a9ab02ac612348b733` | 4,674,160 |
| RabbitMQ | `d12d51e73ddb2d36a772c5038818cb0802c41c43998815dc8328f3479ed91dda` | 6,292,944 |

There is **no source-SHA or FFI ABI annotation in these OCI artifacts**. Do not
invent one or mistake the metadata's SDK **crate** version `0.11.3` for FFI ABI
`0.14.0`. The authorization binding is the reviewed immutable digest plus the
original official upload record, not an unsigned annotation or a mutable tag.
The helper checks the original run/job records and the hash and ABI constant of
`components/plugin-sdk/src/ffi/metadata.rs` at that source commit. It validates
the actual OCI metadata (including SDK/core/lib versions), every descriptor and
blob checksum/size, and the binary's arm64 dylib header and macOS
`LC_BUILD_VERSION` platform without loading it.
The original log review is recorded in the inventory; execution does not depend
on downloading an expiring Actions log or claim new build provenance.

Extending the inventory requires code review of the provenance and immutable
binary/metadata pins, and updating the workflow's batch choices and execute
allowlist. The helper currently accepts only this macOS arm64 reaction format.
It does not accept registry, tag, digest, manifest-path or key overrides.

## Operator procedure

1. After the implementation is merged, open **Actions > Publish Plugins >
   Run workflow** in `drasi-project/drasi-core`. Select branch **main**.
   Set `repair_batch` to `release-2026-09-25-darwin-arm64` and leave
   `repair_action` at its default **plan**. Leave all normal publication inputs
   at their defaults, including the empty `ref`/tag fields.
2. Inspect the read-only job summary and `plugin-signature-repair-plan` JSON
   artifact. Confirm both immutable references, source commit, versions,
   platform, binary checksums/sizes and `inventory_validated: true`.
   `would-sign` / `absent` is expected for a legitimate unsigned artifact;
   `already-valid` / `verified` requires no repair. `plan-ready` does **not**
   mean unsigned artifacts are fixed.
3. Only after accepting the plan, run **the same workflow from main** with the
   same batch and explicitly choose `repair_action: execute`. The execute job
   repeats the complete preflight; it does not trust the earlier plan output.
   It uses only the workflow's main commit, never the publication `ref` input.
4. Inspect `plugin-signature-repair-execute` and the summary. Success requires
   every selected artifact to be `already-valid` or `repaired`, with
   `verification: verified`, and `all_verified: true`. Keep the report with the
   operational incident. Independently verify anonymously before considering
   #970 resolved or unblocking dependent releases.

`repair_batch: none` preserves normal dispatch behavior. `workflow_call` has no
repair inputs and continues normal publication. Repair selection suppresses all
normal build, publish, plugin-directory and package-visibility jobs.
Invalid selections fail rather than falling back to publishing.

Plan/selection jobs have only `contents: read`; public checks use anonymous
pull-scoped GHCR tokens, not `PACKAGES_ADMIN_TOKEN`. Execute has only
`contents: read`, `packages: write`, and `id-token: write`, uses the normal
`GITHUB_TOKEN`/`cosign login --password-stdin` pattern in an owned temporary
Docker config, and obtains the keyless certificate from GitHub Actions OIDC.
Verification uses a separate empty Docker config and no login/OIDC credentials.
Repeated executions of the same batch serialize without cancelling a running
signer; unrelated publishing and CI are not serialized by this guard.

## Independent verification

Use pinned cosign `v2.5.2` (the version used by the original publisher and the
repair job). In a fresh shell without registry or Sigstore overrides, use an
empty temporary Docker config for anonymous verification:

```bash
verify_dir="$(mktemp -d)" || exit 1
verify_status=0
(
  export DOCKER_CONFIG="$verify_dir"
  for ref in \
    ghcr.io/drasi-project/reaction/sse@sha256:60c70c051c53c423caa9e60d55f09d01fbdea853ff202450412bdc06cf262ad5 \
    ghcr.io/drasi-project/reaction/rabbitmq@sha256:87327fac1978a58f0aace2a5059d4e169c2245f0cd2cde62c33e47d18966677c
  do
    cosign --timeout 120s verify \
      --certificate-identity 'https://github.com/drasi-project/drasi-core/.github/workflows/publish-plugins.yml@refs/heads/main' \
      --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
      "$ref" || exit 1
  done
) || verify_status=$?
rm -r -- "$verify_dir"
test "$verify_status" -eq 0
```

Also use a compatible drasi-server build with dynamic plugins on **macOS arm64**,
using its **default signature-verifying installer**, no unsigned-plugin override
and no relaxed identity policy. Use a fresh, isolated plugin directory rather
than an active server's plugins:

```bash
install_dir="$(mktemp -d)" || exit 1
drasi-server --plugins-dir "$install_dir" plugin install reaction/sse:0.3.8
drasi-server --plugins-dir "$install_dir" plugin install reaction/rabbitmq:0.1.6
shasum -a 256 "$install_dir/libdrasi_reaction_sse.dylib" \
  "$install_dir/libdrasi_reaction_rabbitmq.dylib"
```

Inspect the signature results and `plugins.lock` in that directory: both
signatures must be verified with the issuer/subject above, the resolved manifest
digests must match the inventory, and the downloaded checksums must match the
table. Installer exit status alone is not that evidence. Retain this isolated
directory until the results are recorded, then remove only that directory.
Do not change a server's trust configuration, update its stack or use a
platform override to make recovery pass. The cosign commands are the independent
exact-identity check, not a substitute for this default installer check.

## Failures, retries and rollback

The repair pins both the cosign installer action and binary version. It first
verifies the exact official identity and issuer. Only cosign v2.5.2's specific
no-signatures response **plus** proven absence at all supported signature
locations authorizes signing. The host supports legacy `.sig` and bundle tags;
an existing bundle/referrer that the pinned verifier cannot verify is a stop,
not permission to overwrite it. An empty, well-formed OCI referrers index also
counts as absence.

Missing artifacts, provenance/metadata/digest/size/target mismatches, unexpected
404 bodies, authentication failures, malformed or wrong-signer signatures and
unrecognized verifier errors stop the operation. Network/HTTP transient errors
and signing/verification failures have at most three attempts, two seconds
between attempts, 30-second HTTP and 120-second cosign timeouts, plus process
deadlines and a 30-minute job limit. A final signing or verification failure
is nonzero; unattempted artifacts remain blocked in the report. Command names,
immutable references, exits/timeouts and sanitized errors are retained, not
tokens, credential response bodies or raw cosign stderr.

Execute writes only public cosign signature attachments (legacy `.sig` format)
and transparency-log entries. It never rebuilds, uploads a plugin binary,
retags a release, updates a directory/catalog or changes package visibility.
A partially successful run may leave a valid signature on an earlier artifact;
a repeat run verifies and skips it. A timeout may also have completed a write,
so inspect/verify before retrying. **Rollback is not deleting binaries or
moving release tags.** Stop and investigate unexpected signing evidence under
the project's incident process; this helper does not delete signatures.

Offline regression coverage, including parsed workflow gates and process exit
codes, runs with Python's standard library and Ruby's standard YAML parser:

```bash
python3 .github/scripts/repair-plugin-signatures.test.py
```
