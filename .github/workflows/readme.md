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

### [release-plz.yml](release-plz.yml)
- **Purpose**: Automates version bumps, changelog generation, and crate publishing using release-plz.
- **Triggers**:
  - Automatically on push to `main` branch
  - Manual workflow dispatch with optional dry-run mode

#### Automatic Behavior (Push to Main)

The workflow detects the type of commit and runs the appropriate action:

| Commit Type | Detection | Action |
|-------------|-----------|--------|
| Regular commit | Commit message does NOT start with `chore: release` | Creates/updates a Release PR with version bumps and CHANGELOGs |
| Release PR merge | Commit message starts with `chore: release`, `chore(release)`, or `release:` | Publishes crates to crates.io and creates git tags |

#### Manual Trigger

| Input | Effect |
|-------|--------|
| `dry_run = false` (default) | Same as automatic - detects commit type and runs appropriate action |
| `dry_run = true` | Preview mode - shows what versions would be bumped and what crates would be published without making any changes |
| `force_publish = true` | Force publish crates to crates.io, bypassing commit message detection. Use this to recover from a failed release where the commit message doesn't match the expected pattern |

#### Recovering from Failed Releases

If a release PR was merged but publishing failed (or the commit message didn't match the expected pattern), you can manually trigger publishing:

1. Go to **Actions** → **Release-plz** → **Run workflow**
2. Check the **"Force publish crates"** checkbox
3. Click **Run workflow**

This will run `release-plz release` which publishes all crates that have local versions newer than what's on crates.io.

#### Release Flow

1. **Merge feature/fix PRs to main** → Workflow creates a "Release PR" with:
   - Version bumps based on conventional commits
   - Updated CHANGELOG.md files
   - Updated dependency versions

2. **Review the Release PR** → Check the proposed version bumps and changelog entries

3. **Merge the Release PR** → Workflow detects the release commit and:
   - Publishes all updated crates to crates.io
   - Creates git tags for each published version
   - Creates GitHub releases

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

### [scorecard.yaml](scorecard.yaml)
- **Purpose**: Runs OpenSSF Scorecard analysis to evaluate repository security and best practices.
- **Triggers**:
  - Pushes to `main`.
  - Scheduled weekly (every Monday at 15:15 UTC).

### [test.yml](test.yml)
- **Purpose**: Executes `cargo test` to run unit tests and performance tests.
- **Trigger**: Automatically triggered on pull requests to the `main`, `feature/*`, or `release/*` branches and pushes to the `main` branch.
  - Note: Performance tests are skipped for draft pull requests.


## Viewing Workflow Status

Navigate to the **Actions** tab in your repository to view the status, logs, and results of each workflow run.