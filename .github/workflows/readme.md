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
  - Automatically triggered on pull requests to the `main` branch and any pushes.

### [coverage.yaml](coverage.yaml)
- **Purpose**: Generates and uploads code coverage reports to Codecov.
- **Trigger**: Automatically triggered on pull requests to the `main` branch and pushes to the `codecov-test` branch.

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
- **Trigger**: Automatically triggered on pull requests and pushes to the `main` branch.


## Viewing Workflow Status

Navigate to the **Actions** tab in your repository to view the status, logs, and results of each workflow run.