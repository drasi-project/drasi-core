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
- **Nightly behavior**: `nightly.yml` runs `release-plz update` (no crate publication), then publishes unsigned plugins with `sign: false`, `dry_run: false`, `skip_visibility: true`, and a fixed `drasi-nightly-test` tag by default. A passing nightly does not validate the signed, versioned release's visibility checks.

### [scorecard.yaml](scorecard.yaml)
- **Purpose**: Runs OpenSSF Scorecard analysis to evaluate repository security and best practices.
- **Triggers**:
  - Pushes to `main`.
  - Scheduled weekly (every Monday at 15:15 UTC).

### [test.yml](test.yml)
- **Purpose**: Runs the mocked package-visibility and release-retry shell regressions before the Rust dependency-cycle check and unit/integration tests. The visibility mock rejects non-GET requests; no real credentials or API writes are used.
- **Trigger**: Automatically triggered on pull requests to `main`, `feature/*`, `feature-lib`, or `release/*`, and by manual dispatch.


## Viewing Workflow Status

Navigate to the **Actions** tab in your repository to view the status, logs, and results of each workflow run.