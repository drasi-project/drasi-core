# GitHub Actions Workflows

This document describes the GitHub Actions workflows in the `.github/workflows` directory and how to trigger them.

## Agentic Workflows

Some workflows in this directory use GitHub Agentic Workflows (gh-aw), which are AI-powered workflows that can autonomously perform tasks like issue research and code analysis.

### Working with Agentic Workflows

To modify or create agentic workflows, you'll need to:

1. **Upgrade GitHub CLI** (if needed):
   ```bash
   gh --version  # Check your current version
   # Upgrade if needed (instructions vary by OS)
   ```

2. **Install or upgrade the gh-aw extension**:
   ```bash
   gh extension install githubnext/gh-aw
   # Or upgrade if already installed:
   gh extension upgrade githubnext/gh-aw
   ```

3. **Edit the source `.md` files** (e.g., `drasi-issue-researcher.md`):
   - These files define the AI agent's behavior and permissions
   - Do NOT edit the `.lock.yml` files directly

4. **Compile the workflows**:
   ```bash
   cd .github/workflows
   gh aw compile drasi-issue-researcher.md
   # Repeat for each .md file (except readme.md)
   ```

   This generates or updates the corresponding `.lock.yml` file that GitHub Actions actually runs.

5. **Commit both files**:
   ```bash
   git add drasi-issue-researcher.md drasi-issue-researcher.lock.yml
   git commit -m "Update drasi-issue-researcher workflow"
   ```

### Available Agentic Workflows

#### [drasi-issue-researcher.md](drasi-issue-researcher.md)
- **Purpose**: Automatically researches GitHub issues labeled with "needs-research" and posts a comprehensive Research Brief
- **Trigger**: When a "needs-research" label is applied to an issue
- **Output**: Posts a detailed comment with problem analysis, relevant code locations, proposed approaches, and acceptance criteria

## Standard Workflows

### [ci-lint.yml](ci-lint.yml)
- **Purpose**: Runs linting checks to ensure code quality and adherence to coding standards.
- **Trigger**: 
  - Automatically triggered on pull requests to the `main`, `feature/*`, or `release/*` branches and any pushes.

### [coverage.yaml](coverage.yaml)
- **Purpose**: Generates and uploads code coverage reports to Codecov.
- **Trigger**: Automatically triggered on pull requests to the `main`, `feature/*`, or `release/*` branches and pushes to the `codecov-test` branch.

### [devskim.yml](devskim.yml)
- **Purpose**: Performs security analysis using DevSkim to detect potential vulnerabilities.
- **Triggers**:
  - Pushes to `main`.
  - Pull requests targeting `main`.
  - Scheduled weekly (every Sunday at 00:30 UTC).

### [pr-assignment-check.yml](pr-assignment-check.yml)
- **Purpose**: Ensures external contributors have a linked issue and are assigned to it before submitting a PR. Maintainers (write/admin access) are exempt.
- **Trigger**: `pull_request_target` on `opened`, `reopened`, or `edited`.
- **Behavior**:
  - If no linked issue is found, adds the `needs-issue` label and comments with instructions.
  - If the author is not assigned to the linked issue, closes the PR with an explanation.

### [pr-first-approval-label.yml](pr-first-approval-label.yml) + [pr-first-approval-label-run.yml](pr-first-approval-label-run.yml)
- **Purpose**: Manages the `need-2nd-review` label based on PR approval count — adds it on the first approval and removes it once a second approval is received.
- **Trigger**: `pull_request_review` on `submitted` (approval only).
- **Design**: Uses a two-stage `workflow_run` pattern because the `pull_request_review` event provides only a read-only `GITHUB_TOKEN` for fork PRs:
  1. **Stage 1** (`pr-first-approval-label.yml`): Saves the PR number as an artifact.
  2. **Stage 2** (`pr-first-approval-label-run.yml`): Runs in the base-repo context with write permissions, re-queries reviews from the API, and manages the label.

### [release-plz.yml](release-plz.yml)
- **Purpose**: Automates version bumps, changelog generation, and crate publishing using release-plz, then calls `publish-plugins.yml` for signed multi-architecture plugin OCI images.
- **Triggers**:
  - Automatically on push to `main` branch
  - Manual workflow dispatch with optional dry-run mode

#### Automatic Behavior (Push to Main)

The workflow detects the type of commit and runs the appropriate action:

| Commit Type | Detection | Action |
|-------------|-----------|--------|
| Regular commit | Commit message does NOT start with `chore: release` | Creates/updates a Release PR with version bumps and CHANGELOGs |
| Release PR merge | Commit message starts with `chore: release`, `chore(release)`, or `release:` | Verifies public plugin directory visibility, publishes crates to crates.io and creates git tags, then publishes signed plugin OCI images |

#### Manual Trigger

| Input | Effect |
|-------|--------|
| `dry_run = false` (default) | Same as automatic - detects commit type and runs appropriate action |
| `dry_run = true` | Preview mode - shows what versions would be bumped and what crates would be published without making any changes |
| `force_publish = true` | Force publish crates to crates.io, bypassing commit message detection. Use this to recover from a failed release where the commit message doesn't match the expected pattern |

#### Recovering from Failed Releases

If a release PR was merged but publishing failed (or the commit message didn't match the expected pattern), start a new run:

1. Merge any workflow/script repair first. Verify that the crate versions on the corrected `main` are the intended release versions, comparing them with crates.io; do not bump versions or revert the release PR just to retry publishing.
2. Confirm the [public package setup](#public-package-setup) is complete for existing packages.
3. Go to **Actions** → **Release-plz** → **Run workflow** and select the corrected `main`.
4. Set **Force publish crates** (`force_publish`) to `true` and **Dry run only** (`dry_run`) to `false`, then click **Run workflow**.

This runs `release-plz release` for unpublished crate versions and, on success, calls `publish-plugins.yml` with signing enabled. **Re-running the old failed run uses the old workflow revision, not the repair on `main`.** These are manual recovery instructions; changing the workflow alone does not publish anything.

New packages may be private after their first push. If final visibility verification fails, a package administrator must make them public in GitHub Package settings, then rerun the failed verification job. If only plugin publishing failed after crates were published, it can also be retried via a new `publish-plugins.yml` dispatch at the intended release ref without republishing crates.

Plugin publication has separate artifact and catalog outcomes. A required upload,
signing, verification, or catalog error fails the command and Actions job, even
when other entries succeeded. Artifact failures suppress that batch's catalog
updates. Catalog failures do **not** roll back uploaded plugins or successful
catalog writes. Inspect the failed entry and retained partial-publication report;
do not interpret an upload message as a complete release or change versions just
to repair a catalog. Existing signature-only repair remains a separate operation.

Every successful push remains in `publication.json` with its reference and
immutable digest, even when signing or verification subsequently fails.
`signed` and `verified` report confirmed completion of those respective steps;
`signature_error` records the failing `stage` (`signing` or `verification`) and
its error message, or is null when no signature step failed. For example, a
verification failure retains `signed: true` and `verified: false`. These partial
records are recovery evidence, not successful publication: the command still
exits nonzero, suppresses the batch's catalog updates, and cannot produce a
successful candidate handoff.

#### Shared dependency baseline and prepared candidates

The root `Cargo.lock` is committed. Normal nightly and release preparation use
this same reviewed dependency baseline. `release-plz.toml` sets
`dependencies_update = false`: release-plz reconciles workspace version changes
with `cargo update --workspace`, rather than refreshing every external dependency.
Workspace entries, or dependencies whose changed constraints require it, may
still change. Generated release PRs must include the corresponding lock changes.
Review their manifest and lock diffs together.

To intentionally update dependencies, run `cargo update` (or
`cargo update -p <package> --precise <version>`), review and commit `Cargo.lock`
together with any manifest changes, and run the normal tests and release
rehearsal. Do not delete the lockfile as a workaround for a locked-build error.
The initial committed lock establishes a new baseline; it is not a reconstruction
of the dependencies used in an older release.

Before fan-out, the workflows retain `source.tar` and `candidate.json`. The
candidate records the source commit, prepared-source SHA-256, baseline and
prepared lock hashes, toolchain, mode, package and FFI ABI versions, and expected eight-target plugin
inventory. Nightly snapshots the tree **after** release-plz prepares versions and
changelogs. Production snapshots the release tree before crate publication.
Standalone plugin dispatch snapshots once and accepts only refs from trusted
main history. It first checks out the official repository at the triggering
workflow SHA, resolves the requested branch, tag, or full commit SHA without
checking it out, and validates ancestry against the exact
`refs/remotes/origin/main` reference. Only the validated immutable commit is then
checked out (detached) and frozen. This avoids branch/tag names shadowing the
trusted-main reference, mutable-ref races, and untrusted source checkout before
validation. An empty `ref` keeps the triggering commit; older main-history
revisions remain supported only when their built artifacts satisfy the current
publication ABI contract. Pre-ABI-metadata revisions cannot be republished by
selecting their old `xtask`. Nightly, production publication, and repair must run from the
official repository's main; there is no branch-testing exception.

Each target restores the same bundle and validates its hashes before building.
Restore exports the candidate's recorded `source_commit` as a step output.
Plugin builds pass it to the SDK as `DRASI_BUILD_GIT_SHA`, including explicit
manylinux container forwarding and cross environment passthrough. Local crate
package verification receives the same value. The SDK embeds this full SHA in
`PluginMetadata.git_commit` instead of discovering a potentially newer tooling
checkout or depending on `.git` being mounted. The candidate hash still identifies
the prepared manifests/changelogs on top of that source commit.
Metadata, outer xtask invocations, nested Cargo/cross builds, and manylinux builds
use `--locked`. Source/lock checks also run after the work. Missing or inconsistent
locks fail; targets never regenerate them. Different targets still compile their
appropriate dependency subsets. This freezes Rust inputs, not OS packages or
byte-for-byte output binaries, and does not impose our lock on downstream crate
consumers.

Plugin compilation runs from the restored candidate, but publication and dry-run
preflight always run the **trusted workflow checkout's** `xtask`, using that
checkout's committed lockfile and an explicit path to the candidate's built
plugins. The publisher validates every artifact's required ABI metadata and
binary marker before reading publication credentials or uploading any artifact.
A selected historical publisher therefore cannot bypass the current checks.
Missing or inconsistent ABI metadata fails before upload, even for a batch
containing both valid and legacy artifacts. No marker or annotation is inferred,
repaired, or synthesized for an old binary.

The candidate retains the ABI declared by the selected source's
`FFI_SDK_VERSION`, independently of the workflow tooling's own ABI and package
versions. Target reports and the final handoff require that exact `abi_version`;
absent ABI values or even patch-level report disagreements fail validation.
Older prepared bundles without this field require a new explicit preparation.
This report check is additional evidence, not a substitute for pre-upload
artifact validation.

Candidate artifacts are retained for 90 days and named by run ID and attempt.
For a nightly retry, rerun **all jobs** so the attempt's candidate, packages, tags,
and reports agree; a partial rerun must not borrow a different attempt's outputs.
Expired/missing artifacts require a new explicit preparation, not a silent
fallback. The reusable workflow's `candidate_artifact` and `candidate_sha256`
inputs are an internal same-run contract, not arbitrary cross-run download inputs.

#### Release-nightly parity contract

Changes to release preparation, caller inputs, publishing, or safeguards must
review **both** [release-plz.yml](release-plz.yml) and [nightly.yml](nightly.yml)
in the same PR. Keep common behavior in
[publish-plugins.yml](publish-plugins.yml),
[install-release-tools](../actions/install-release-tools/action.yml),
[release-candidate.py](../scripts/release-candidate.py),
[release-plz.toml](../../release-plz.toml), and [xtask](../../xtask/src/main.rs),
rather than duplicating it between callers.

| Area | Production release | Nightly rehearsal |
|------|--------------------|-------------------|
| Version preparation | `release-plz release-pr` prepares reviewed changes; publishing uses the merged release revision | `release-plz update` prepares changes locally, without a PR or commit |
| Dependencies | Committed lockfile baseline; release preparation reconciles workspace versions without a broad refresh | Same configuration and baseline policy; no fresh-dependency mode |
| Prepared candidate | Shared freeze helper runs before crate publication; its artifact and hash feed every plugin target | Same helper runs after version preparation; its artifact and hash feed packaging and every plugin target |
| Crates | `release-plz release` publishes crates and creates official release records; `publish_no_verify = true` skips the isolated verification build | `cargo package --locked` verifies extracted package sources and retains archives; no crates.io writes or official release records |
| Plugins and catalog | Shared eight-target publisher, native/glibc checks, signing, verification, and public visibility checks | Same implementation and required checks, but plugins, catalog, and signatures use the isolated nightly namespace and unique run/attempt plugin tags |
| Completion | Successful crate publication gates plugins; required plugin/catalog failures fail the workflow | Packaging and all publication/visibility checks gate the complete-inventory handoff; downstream runtime E2E remains separate |

Both callers use the trusted workflow checkout's `xtask` for binary-backed ABI
preflight and publication, regardless of the selected build revision. The SDK
package annotation (`io.drasi.plugin.sdk-version`) and FFI ABI annotation
(`io.drasi.plugin.abi-version`) remain distinct. Build metadata obtains the ABI
from each binary's embedded marker, which also backs its exported FFI metadata.
Publication rejects missing markers or disagreement with the JSON metadata
before upload, including dry runs and cross-compiled targets. The same ABI is
included in the OCI annotation and publication report. These shared producer
checks do not replace released-host/runtime compatibility checks.

For each changed release step, either rehearse it through the same implementation
or document why it is production-only and how it is validated separately. Do not
weaken nightly signing, visibility, locked-build, or complete-inventory checks to
make it match a failing production path. Reviewers should check the job dependency
gates and caller inputs, not just whether one nightly run is green.

GitHub's reusable-workflow `inputs` context can contain caller-only dispatch
fields. The publication selector accepts the boolean `force_publish` field only
from an official `main` manual release call, without any repair selection.
It is caller metadata, not a plugin-publication or signature-repair override.
Unknown fields, invalid types, and unexpected caller contexts must still fail.
New caller inputs must extend the relevant contract tests instead of disabling
strict validation or silently ignoring arbitrary fields.

The [candidate tests](../scripts/release-candidate.test.py) parse the workflow
YAML and exercise scheduled/manual nightly, automatic release, manual release,
and force-publish recovery selection, and reject non-main publication. They also check
shared candidate/tooling/publication safeguards and intentional preparation and
dry-run differences. The [repair tests](../scripts/repair-plugin-signatures.test.py)
cover strict input validation and repair isolation. Both suites already run in
[test.yml](test.yml). Run them when changing these contracts:

```bash
python3 .github/scripts/release-candidate.test.py
python3 .github/scripts/repair-plugin-signatures.test.py
actionlint .github/workflows/nightly.yml .github/workflows/release-plz.yml .github/workflows/publish-plugins.yml
```

These focused checks do not execute production publication or downstream runtime
E2E, and cannot automatically establish parity for an arbitrary new release step.
Update the table and behavioral tests with deliberate changes; do not merely
change assertions to hide an unintended difference.

#### Release Flow

1. **Merge feature/fix PRs to main** → Workflow creates a "Release PR" with:
   - Version bumps based on conventional commits
   - Updated CHANGELOG.md files
   - Updated dependency versions

2. **Review the Release PR** → Check the proposed version bumps and changelog entries

3. **Merge the Release PR** → Workflow detects the release commit and:
   - Verifies that `drasi-plugin-directory` exists, is readable, and is public
   - Publishes all updated crates to crates.io
   - Creates git tags for each published version
   - Creates GitHub releases
   - Calls `publish-plugins.yml` to publish signed multi-architecture plugin OCI images and verify public package visibility

#### Conventional Commits and Versioning

This project uses [Conventional Commits](https://www.conventionalcommits.org/) to determine version bumps. Commit messages must be prefixed with a type:

```
type: description

# Examples:
fix: resolve null pointer in query parser
feat: add support for PostgreSQL 15
feat!: rename QueryResult to QueryOutput
docs: update installation guide
chore: update dependencies
```

**Pre-1.0 Versioning Strategy**

Until the project reaches 1.0.0, we use a `0.major.minor` versioning scheme (no patch releases):

| Commit Prefix | Meaning | Version Bump Example |
|---------------|---------|---------------------|
| `fix:` | Bug fix | 0.3.1 → 0.3.**2** |
| `feat:` | New feature (non-breaking) | 0.3.1 → 0.3.**2** |
| `feat!:` | Breaking change | 0.3.1 → 0.**4**.0 |
| `docs:`, `chore:`, `test:`, `refactor:` | No version change | 0.3.1 → 0.3.1 |

**Note:** Both `fix:` and `feat:` increment the last number (minor in our scheme) until we release 1.0.0. Use `feat!:` or include `BREAKING CHANGE:` in the commit body for changes that should bump the middle number.

After 1.0.0, standard semantic versioning will apply:
- `fix:` → patch bump (1.2.3 → 1.2.4)
- `feat:` → minor bump (1.2.3 → 1.3.0)
- `feat!:` → major bump (1.2.3 → 2.0.0)

#### Semver Checking

The workflow includes **cargo-semver-checks** which automatically detects breaking API changes:

- Scans for removed public items, changed function signatures, removed struct fields, etc.
- Warns if a patch/minor version bump is proposed but breaking changes are detected
- Helps ensure downstream users won't get surprise compile errors after updating

This runs automatically during the release process - no manual invocation needed.

#### Configuration

- **release-plz.toml**: Controls versioning behavior, changelog generation, and which packages to publish
- **cliff.toml**: Configures changelog format (conventional commits)
- Packages marked with `publish = false` in release-plz.toml are excluded from releases (shared-tests, query-perf, examples)

#### Required Secrets

- `GITHUB_TOKEN`: Automatically provided, used for creating PRs and releases
- `CARGO_REGISTRY_TOKEN`: Must be configured in repository secrets for publishing to crates.io
- `PACKAGES_ADMIN_TOKEN`: PAT classic with `read:packages`, access to the organization's container packages, and organization SSO authorization if required. The existing secret name is retained for compatibility; it is used only for read-only package visibility verification, not administration or publication.

##### Public package setup

GitHub's [Get a package REST endpoint](https://docs.github.com/en/rest/packages/packages#get-a-package) supports reading package metadata; there is no supported `PATCH` visibility endpoint. The workflows use explicit `GET` requests before publishing to verify that `drasi-plugin-directory` exists, is readable, and has `visibility: public`. After all plugin builds and pushes succeed, they verify every published package is public. A successful check proves read access and public visibility, **not package administration rights**.

A package administrator must use each package's **Package settings** → **Change visibility** to select **Public** ([GitHub instructions](https://docs.github.com/en/packages/learn-github-packages/configuring-a-packages-access-control-and-visibility)). This is a manual setup step, including for `drasi-plugin-directory`; newly created plugin packages may need it after their first push. Then rerun verification. The script fails rather than changing visibility or ignoring private/internal packages.

To verify explicitly, run `.github/scripts/package-visibility.sh verify-public drasi-plugin-directory source/http` with `GH_TOKEN` supplied securely from the configured token. `PACKAGE_VISIBILITY_ORG` overrides the default `drasi-project` organization. Missing credentials, unreadable/missing packages, and unexpected visibility responses fail closed. The publication steps still use `GITHUB_TOKEN` with `packages: write`, and signing still requires `id-token: write`.

##### Rotating `PACKAGES_ADMIN_TOKEN`

Repository maintainers are responsible for manual rotation before expiry. GitHub Actions does not renew the token automatically. Rotation is not a fix for an unsupported API endpoint.

1. Check the expiry in [GitHub's classic PAT settings](https://github.com/settings/tokens). Before it expires, create a replacement classic PAT with `read:packages` from an account with read access to the Drasi container packages. Authorize it for organization SSO if required.
2. Replace the repository secret using `gh secret set PACKAGES_ADMIN_TOKEN --repo drasi-project/drasi-core` and enter the token at the hidden prompt. Do not put it in source code, command arguments, or logs.
3. Using the replacement token, verify read access to `drasi-plugin-directory` with `GET /orgs/drasi-project/packages/container/drasi-plugin-directory`, then revoke the previous token.

### [publish-plugins.yml](publish-plugins.yml)
- **Purpose**: Builds and publishes the plugin architecture matrix and verifies public GHCR package visibility without changing it.
- **Triggers**: Called by `release-plz.yml` after crate publication, called by `nightly.yml`, or dispatched manually.
- **Inputs**: `sign` controls cosign signing; `dry_run` skips publication and visibility checks; `skip_visibility` skips the visibility preflight, package listing, and final verification.
- **Candidate inputs**: Internal callers provide the same-run prepared artifact and manifest hash; standalone dispatch prepares one locked candidate before the matrix.
- **Nightly behavior**: Uses the same eight targets, native setup, GNU glibc 2.28 checks, and required cosign signing/verification as production, but writes only to the dedicated test namespace described below.

### [nightly.yml](nightly.yml)

Runs daily at **12:00 UTC (05:00 PDT / 04:00 PST)** on `main`. GitHub's UTC
schedule does not shift with daylight saving time.

Scheduled and manually dispatched nightlies use the committed lockfile baseline
and the same release-plz configuration as release preparation. There is no
dependency-refresh mode; intentional dependency updates must be reviewed and
committed in `Cargo.lock` before the rehearsal. Release-plz still prepares versions
and reconciles workspace lock entries as described above. The candidate and handoff
always record `mode: locked`; nightly does not commit the prepared changes.
There are no skip/reuse-tag inputs that can produce a ready incomplete candidate.

#### Manual rehearsal on main

After merging workflow changes into `main`, start a new rehearsal with:

```bash
gh workflow run nightly.yml --repo drasi-project/drasi-core \
  --ref main
```

Manual dispatch has no inputs. Only the official repository's `main` can run the
rehearsal; forks, other branches, and tags cannot publish. This is a real, full
eight-platform signed publication to the isolated nightly namespace, not a dry
run. The package provisioning requirements below still apply.

Signatures use the exact `publish-plugins.yml@refs/heads/main` identity. The
schema-version-1 candidate and handoff retain `source_ref: refs/heads/main` and
`branch_test: false`; the latter is no longer configurable. Earlier branch-test
artifacts remain diagnostic outputs, and the handoff rejects them rather than
treating them as trusted-main candidates.

The sequence is preparation, verified local packaging, eight-target signed plugin
publication plus public visibility checks, then complete-inventory validation.
Packaging uses Cargo's supported multi-package selection for all publishable,
release-enabled workspace crates, including their unpublished dependency versions.
Internal `publish = false` and release-plz exclusions remain excluded. Cargo checks
the dependency closure and builds extracted `.crate` sources; verification is
never disabled. Package-local normalized lockfiles are outputs, not replacements
for the frozen root build lock. The verified `.crate` files and checksums are
retained as Actions artifacts; nothing is published to crates.io.

Nightly OCI repositories are under `ghcr.io/drasi-project/drasi-core-nightly`,
including `drasi-core-nightly/drasi-plugin-directory`. Tags are
`nightly-<run_id>-<run_attempt>-<architecture>`. Cosign signatures stay alongside
these test repositories; signature repository redirection is disabled. Official
plugin repositories, catalogs, version tags and GitHub Releases are unchanged.

Before enabling this path, an administrator must provision the test directory
package and grant the workflow access. The directory must be public; newly
created test plugin packages may also require the administrator's public-visibility
step. The existing `PACKAGES_ADMIN_TOKEN` is used only for GET checks of
namespace-qualified names. Missing access or visibility fails the rehearsal;
the workflow does not change credentials or package settings.

#### Exact producer handoff to test-infra

After all required stages pass, the run uploads
`nightly-ready-<run_id>-<run_attempt>` containing `ready.json` (schema version 1).
It identifies the producer repository/run/attempt, source commit and prepared
source hash, candidate artifact and manifest hash, lock hash, mode, toolchain,
crate/SDK/core/lib versions, package checksums, all eight plugin target inventories,
immutable OCI digest references, and the exact cosign identity and issuer.

The consumer must receive an **explicit producer run ID and attempt**, check
through GitHub's API that that nightly attempt completed successfully on trusted
main, and download its named ready artifact and candidate bundle. It must compare
hashes, build a deliberately pinned compatible host against the prepared source,
install the recorded digests without local-binary or mutable-tag fallback, verify
signatures under the recorded publisher policy, and associate results with this
candidate. A ready artifact from an unfinished or failed run is insufficient.

This consumer interface is implemented by
[drasi-project/test-infra#103](https://github.com/drasi-project/test-infra/issues/103).
The existing test-infra workflow does **not** yet accept the full contract; core
does not dispatch it with unsupported inputs or add cross-repository credentials.
Its merged [coverage](https://github.com/drasi-project/test-infra/pull/93) is the
Linux x86_64 HTTP/gRPC standard/adaptive subset, not every plugin on eight platforms.
Until that consumer integration and an authorized end-to-end run complete,
producer readiness is **not runtime E2E success**. A green core nightly validates
packaging and signed test publication, not crates.io writes/quotas or production
release creation.

### [scorecard.yaml](scorecard.yaml)
- **Purpose**: Runs OpenSSF Scorecard analysis to evaluate repository security and best practices.
- **Triggers**:
  - Pushes to `main`.
  - Scheduled weekly (every Monday at 15:15 UTC).

### [test.yml](test.yml)
- **Purpose**: Runs the mocked package-visibility and release-retry shell regressions before the Rust dependency-cycle check and unit/integration tests. The visibility mock rejects non-GET requests; no real credentials or API writes are used.
- **Trigger**: Automatically triggered on pull requests to `main`, `feature/*`, `feature-lib`, or `release/*`, and by manual dispatch.

### [test-integration.yml](test-integration.yml)
- **Purpose**: Runs `#[ignore]`d plugin integration and end-to-end tests that start containers. The package list is discovered from the workspace (`cargo run -p xtask -- list-integration-test-matrix`), so new crates are included automatically. Live-network tests that need credentials are not included.
- **Trigger**:
  - Pull requests to `main`, `feature/*`, `feature-lib`, or `release/*` (non-draft).
  - Manual `workflow_dispatch`.
- **Notes**: Default `cargo test` in [test.yml](test.yml) skips these tests. Run locally with `cargo test -p <crate> -- --ignored`.


## Viewing Workflow Status

Navigate to the **Actions** tab in your repository to view the status, logs, and results of each workflow run.