# GitHub Actions Workflows

This document describes the GitHub Actions workflows in the `.github/workflows` directory and how to trigger them.

## Workflows

### [automerge.yml](automerge.yml)
- **Purpose**: Automatically merges Renovate dependency update PRs after a waiting period based on the update type (patch vs minor versions).
- **Triggers**:
  - Scheduled to run every Wednesday at 12:00 PM Pacific Time (19:00 UTC)
  - Can be manually triggered via workflow dispatch


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

### [scorecard.yaml](scorecard.yaml)
- **Purpose**: Runs OpenSSF Scorecard analysis to evaluate repository security and best practices.
- **Triggers**:
  - Pushes to `main`.
  - Scheduled weekly (every Monday at 15:15 UTC).

### [test.yml](test.yml)
- **Purpose**: Executes `cargo test` to run unit tests.
- **Trigger**: Automatically triggered on pull requests to the `main`, `feature/*`, or `release/*` branches and pushes to the `main` branch.

### [release.yml](release.yml)
- **Purpose**: Publishes crates to crates.io for both core libraries and individual components.
- **Trigger**: Manual via workflow dispatch
- **Environment**: Requires `drasi-core-release` environment with `CARGO_REGISTRY_TOKEN` secret
- **Inputs**:
  - `release_target`: Choose between `core-and-lib` (releases all core libraries) or `component` (releases a single component)
  - `component_name`: Select from dropdown of available components (required when `release_target` is `component`)
  - `version_bump`: Choose `patch`, `minor`, `major`, or `custom` version bump
  - `custom_version`: Specify exact version (only when `version_bump` is `custom`)
  - `dry_run`: Set to `true` (default) to preview changes without publishing, or `false` to publish to crates.io
- **Features**:
  - Runs on `ubuntu-latest-8-cores` for faster builds
  - Uses cargo-release 0.25.20 with `--locked` flag for dependency stability
  - Requires environment approval before execution
  - Provides detailed summary of release outcome

### [test-release.yml](test-release.yml)
- **Purpose**: Tests the release process safely without publishing to crates.io.
- **Trigger**: Manual via workflow dispatch
- **Inputs**:
  - `test_branch`: Branch to test on (will be modified with commits and tags)
  - `release_target`: Choose between `core-and-lib` or `component`
  - `component_name`: Component to release (if `release_target` is `component`)
  - `version_bump`: Choose `patch`, `minor`, or `major`
- **Note**: Always uses `--no-publish` flag to prevent actual publication to crates.io. Creates commits and tags on the test branch for verification.


## Viewing Workflow Status

Navigate to the **Actions** tab in your repository to view the status, logs, and results of each workflow run.