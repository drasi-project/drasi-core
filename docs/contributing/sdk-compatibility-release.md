# SDK package compatibility release

The SDK package line and the FFI ABI are different contracts. SDK, host SDK, and
FFI primitives **0.12.0** use FFI ABI **0.14.0**. This release does not change that
ABI. It separates rebuilt plugins from the 0.11.x package line, which previously
contained incompatible ABIs. Core **0.5.10** and lib **0.9.3** remain unchanged.

Until [#989](https://github.com/drasi-project/drasi-core/issues/989) introduces
explicit registry ABI metadata, every ABI major/minor change must also change
the SDK package major/minor line. Rebuild all dynamic plugins under new, unused
package/OCI versions, and release their SDK-dependent wrapper crates too.
Never relabel an old binary or overwrite an existing version. The registry
continues to require matching SDK/core/lib major.minor annotations, including
for exact pins; the loader independently validates the actual FFI ABI.
For migration, resolve new locks into a fresh plugin directory. Cached binaries
and preexisting installation locks can bypass normal OCI resolution; this
release does not harden those installer paths. A 0.11.x package binary reporting
ABI 0.14.x can still be ABI-compatible with the new loader. Package-line
rejection is a normal-registry-resolution guarantee, not universal raw-loader
rejection.

## Preparation and review

The version changes were generated with the repository-pinned
`release-plz set-version`. SDK package versions are now explicit, as emitted by
release-plz's independent-versioning tooling; `workspace.package.version` is
only the fallback for the remaining internal fixture. Historical changelog
entries must not be renamed. The existing release-plz release PR owns generation
of new changelog entries.

Run from the repository root with Python 3.11+:

```sh
python3 .github/scripts/check-sdk-release.test.py
python3 .github/scripts/check-sdk-release.py --base <base-sha> \
  --write-map /tmp/sdk-release-version-map.json
python3 .github/scripts/check-sdk-release.py --base <base-sha> --check-unused
```

The check inventories every workspace dynamic plugin using xtask's name/feature
convention, follows normal/build dependencies into publishable wrappers, checks
explicit dependency requirements (including standalone examples), and checks
the ABI/package boundary. The optional network check reads crates.io and every
GHCR repository's tags, including pagination and all platform suffixes. It fails
closed on inaccessible repositories. Availability is a point-in-time result,
not a reservation: rerun immediately before approving publication.

The initial 0.12.0 candidate contains **72 changed packages**, **60 dynamic
plugins**, and **66 crates.io-eligible changed packages**. The map records
per-package old/new versions and eligibility. Unchanged packages retain their
existing registry checksums and source commit identities; do not label their
archives with this release's source commit.

## Release order and existing work

1. Review this compatibility change alongside the locked-candidate safeguards
   in [#987](https://github.com/drasi-project/drasi-core/pull/987). Do not replace
   that PR's candidate preparation, lockfile, or eight-target rehearsal.
2. Regenerate the existing release-plz PR
   [#969](https://github.com/drasi-project/drasi-core/pull/969) after this change.
   Its old 0.11.4 SDK candidate must not be published. Recheck the final map if
   other pending changes require additional dependency releases.
3. With explicit release approval, publish crates in dependency order:
   FFI primitives before plugin SDK before host SDK/plugins; Kubernetes common
   before its source/bootstrap; plugins before their dependent wrappers.
   Unchanged core/lib and other dependencies already exist on crates.io.
4. The normal `release-plz.yml` release job invokes `publish-plugins.yml` only
   after successful crate publication. That workflow builds all discovered
   plugins, publishes platform-suffixed versions, signs with cosign, and verifies
   signatures. Use the same reviewed source candidate, no tag override, and
   normal signature verification. A normal non-release commit only updates the
   release PR; no publication was dispatched as part of this preparation.
5. After registry availability, the coordinated server release can generate
   real Cargo checksums, reviewed per-package provenance, and signed OCI locks.
   Then update tutorial pins to that published matching set. Do not fabricate
   checksums/digests or commit an unresolved downstream release lockfile.

Main currently ignores Cargo.lock. The lock/candidate ownership change belongs
to #987. Local verification generates a workspace lock without committing a
competing dependency refresh. Final production builds must use the approved
locked candidate, not an independently resolved dependency graph.

## Exclusions and target limits

The OCI and crates.io inventories are deliberately not identical today:

| Packages | Existing limitation |
| --- | --- |
| `drasi-reaction-aws-sqs` | Excluded by release-plz; still discovered for OCI. Its new OCI version cannot be represented as a new crates.io release until that exclusion is resolved. |
| HERE Traffic source/bootstrap and Open511 source/bootstrap | `publish = false`; still discovered for OCI. |
| `drasi-identity-test` | Internal `publish = false` fixture, but xtask currently discovers and publishes its dynamic artifact. |
| Snapshot-test reaction | Internal fixture without the `dynamic-plugin` discovery feature; not an OCI release plugin. |
| Indexes, state stores, WALs, application/platform wrappers | Static crates, not separate discovered dynamic plugins. SDK-dependent wrappers/indexes still require new crate versions. |

The production matrix targets Linux GNU x86_64/ARM64 (glibc <= 2.28), Linux musl
x86_64/ARM64, Windows GNU/MSVC x86_64, and macOS x86_64/ARM64. Cross-OS builds
from macOS are unsupported by xtask; cross uses Linux/Docker. This preparation
does not claim all eight targets were built locally. `publish-plugins.yml` has
`ref`, `tag`, `pre_release`, and `sign` inputs, but no single-plugin selector.
The separate single-plugin workflow proposal #813 is not assumed merged.

## Kubernetes crypto regression

`export_plugin!` already installs Ring inside each loaded cdylib before runtime
and descriptor construction. The shared Kubernetes client now also selects
Ring before kube constructs Rustls, covering both source and bootstrap and
direct/static use. A preinstalled provider is preserved; kubeconfig and API
errors still propagate.

```sh
cargo test -p drasi-kubernetes-common --test client_crypto_provider
cargo build -j 2 --lib -p drasi-source-kubernetes \
  --features drasi-source-kubernetes/dynamic-plugin
cargo build -j 2 --lib -p drasi-bootstrap-kubernetes \
  --features drasi-bootstrap-kubernetes/dynamic-plugin
cargo test -p drasi-host-sdk --test kubernetes_crypto_provider_test -- --ignored
cargo test -p drasi-host-sdk --features registry --lib registry::resolver::tests
```

The direct-client regression enables both Rustls providers, reproducing the
ambiguous-provider panic without the fix. The cdylib smoke loads real source
and bootstrap binaries, constructs clients in both independent modules, and
requires real Kubernetes authorization errors from a task-owned loopback API
to cross FFI intact. Missing binaries fail, rather than silently skipping.
Set `DRASI_TEST_PLUGIN_DIR` for a non-default artifact directory.
