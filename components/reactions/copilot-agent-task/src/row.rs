// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The launch-query row schema and its allowlist validation.
//!
//! The reaction subscribes to a single "launch query" and acts **only** on
//! `Add` result diffs (see [`crate::reaction`]). Each added row must contain
//! the fields below; anything else causes the row to be rejected (fail-closed
//! — the reaction never launches on malformed or disallowed input).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// One row of the launch query's result set.
///
/// # Field notes / adaptations
///
/// * `profile_ref` is a `"<path>@<blobSha>"` encoded string: the repository
///   path of the agent-profile file, and the git blob SHA it is expected to
///   resolve to at `base_ref`. This is the smallest self-contained way to
///   carry a "pinned file content" reference in a single query column.
/// * `issue_content_version` is compared, during preflight, against a SHA-256
///   digest of the issue body fetched live from GitHub (see
///   [`crate::github::content_version_of`]). GitHub issues have no native
///   "content version" concept, so the reaction and the upstream router are
///   expected to agree on this hash convention as the optimistic-concurrency
///   token for "has the issue changed since the routing decision was made".
/// * `expected_project_status` is the Project (v2) single-select `Status`
///   field value the row was observed with when the launch query emitted it.
///   Preflight re-reads the live status and requires an exact match, so a
///   status change racing the launch aborts the launch instead of proceeding
///   on stale information.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRow {
    /// `"owner/repo"`.
    pub repository: String,
    pub issue_number: u64,
    pub issue_url: String,
    pub issue_node_id: String,
    pub project_item_node_id: String,
    pub route_id: String,
    pub responsibility_id: String,
    /// Opaque version token for the issue body (see struct docs).
    pub issue_content_version: String,
    pub agent_profile: String,
    /// `"<path>@<blobSha>"`.
    pub profile_ref: String,
    pub requested_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    pub required_event_type: String,
    pub expected_event_id: String,
    pub base_ref: String,
    /// See struct docs — an adaptation beyond the literal field list needed to
    /// make the "Project status expected by input" preflight check concrete.
    pub expected_project_status: String,
}

impl LaunchRow {
    /// Parse a launch row from the raw `data` payload of an `Add` diff.
    pub fn from_json(data: &serde_json::Value) -> Result<Self> {
        serde_json::from_value(data.clone()).context("launch row does not match expected schema")
    }

    /// Split `profile_ref` into `(path, expected_blob_sha)`.
    pub fn profile_path_and_sha(&self) -> Result<(&str, &str)> {
        let (path, sha) = self.profile_ref.rsplit_once('@').with_context(|| {
            format!(
                "profileRef '{}' is not '<path>@<blobSha>'",
                self.profile_ref
            )
        })?;
        if path.is_empty() || sha.is_empty() {
            bail!(
                "profileRef '{}' has an empty path or blob SHA",
                self.profile_ref
            );
        }
        Ok((path, sha))
    }

    /// Split `repository` into `(owner, repo)`.
    pub fn owner_and_repo(&self) -> Result<(&str, &str)> {
        let (owner, repo) = self
            .repository
            .split_once('/')
            .with_context(|| format!("repository '{}' is not 'owner/repo'", self.repository))?;
        if owner.is_empty() || repo.is_empty() {
            bail!(
                "repository '{}' has an empty owner or repo",
                self.repository
            );
        }
        Ok((owner, repo))
    }
}

/// Reasons a row can be rejected by allowlist validation. All variants are
/// **permanent** (fail-closed): retrying the identical row will not change
/// the outcome, so the reaction records the failure and moves on rather than
/// blocking the pipeline.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("repository '{0}' is not in the allowed-repositories list")]
    RepositoryNotAllowed(String),
    #[error("agent profile '{0}' is not in the allowed-profiles list")]
    ProfileNotAllowed(String),
    #[error("requested model '{0}' is not in the allowed-models list")]
    RequestedModelNotAllowed(String),
    #[error("fallback model '{0}' is not in the allowed-models list")]
    FallbackModelNotAllowed(String),
    #[error("malformed row: {0}")]
    Malformed(String),
}

/// Validate a parsed row against the reaction's configured allowlists.
/// Fails closed: an empty allowlist allows nothing.
pub fn validate_row(
    row: &LaunchRow,
    allowed_repositories: &[String],
    allowed_profiles: &[String],
    allowed_models: &[String],
) -> Result<(), ValidationError> {
    if !allowed_repositories.iter().any(|r| r == &row.repository) {
        return Err(ValidationError::RepositoryNotAllowed(
            row.repository.clone(),
        ));
    }
    if !allowed_profiles.iter().any(|p| p == &row.agent_profile) {
        return Err(ValidationError::ProfileNotAllowed(
            row.agent_profile.clone(),
        ));
    }
    if !allowed_models.iter().any(|m| m == &row.requested_model) {
        return Err(ValidationError::RequestedModelNotAllowed(
            row.requested_model.clone(),
        ));
    }
    if let Some(fallback) = &row.fallback_model {
        if !fallback.is_empty() && !allowed_models.iter().any(|m| m == fallback) {
            return Err(ValidationError::FallbackModelNotAllowed(fallback.clone()));
        }
    }
    if let Err(e) = row.owner_and_repo() {
        return Err(ValidationError::Malformed(e.to_string()));
    }
    if let Err(e) = row.profile_path_and_sha() {
        return Err(ValidationError::Malformed(e.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_row() -> LaunchRow {
        LaunchRow {
            repository: "drasi-project/drasi-core".to_string(),
            issue_number: 42,
            issue_url: "https://github.com/drasi-project/drasi-core/issues/42".to_string(),
            issue_node_id: "I_kwDOtest".to_string(),
            project_item_node_id: "PVTI_test".to_string(),
            route_id: "route-1".to_string(),
            responsibility_id: "resp-1".to_string(),
            issue_content_version: "deadbeef".to_string(),
            agent_profile: "issue-validator".to_string(),
            profile_ref: "profiles/issue-validator.yml@abc123sha".to_string(),
            requested_model: "gpt-5".to_string(),
            fallback_model: Some("gpt-4".to_string()),
            required_event_type: "CompletedIssueValidation".to_string(),
            expected_event_id: "evt-1".to_string(),
            base_ref: "main".to_string(),
            expected_project_status: "In Progress".to_string(),
        }
    }

    #[test]
    fn parses_from_json() {
        let row = sample_row();
        let value = serde_json::to_value(&row).unwrap();
        let parsed = LaunchRow::from_json(&value).unwrap();
        assert_eq!(parsed, row);
    }

    #[test]
    fn rejects_malformed_json() {
        let value = json!({"repository": "only-one-field"});
        assert!(LaunchRow::from_json(&value).is_err());
    }

    #[test]
    fn profile_ref_splits_on_last_at() {
        let row = sample_row();
        let (path, sha) = row.profile_path_and_sha().unwrap();
        assert_eq!(path, "profiles/issue-validator.yml");
        assert_eq!(sha, "abc123sha");
    }

    #[test]
    fn profile_ref_without_at_is_rejected() {
        let mut row = sample_row();
        row.profile_ref = "no-at-sign".to_string();
        assert!(row.profile_path_and_sha().is_err());
    }

    #[test]
    fn owner_and_repo_splits() {
        let row = sample_row();
        let (owner, repo) = row.owner_and_repo().unwrap();
        assert_eq!(owner, "drasi-project");
        assert_eq!(repo, "drasi-core");
    }

    #[test]
    fn validate_allows_row_within_allowlists() {
        let row = sample_row();
        let repos = vec!["drasi-project/drasi-core".to_string()];
        let profiles = vec!["issue-validator".to_string()];
        let models = vec!["gpt-5".to_string(), "gpt-4".to_string()];
        assert!(validate_row(&row, &repos, &profiles, &models).is_ok());
    }

    #[test]
    fn validate_fails_closed_on_empty_allowlists() {
        let row = sample_row();
        assert_eq!(
            validate_row(&row, &[], &[], &[]).unwrap_err(),
            ValidationError::RepositoryNotAllowed(row.repository.clone())
        );
    }

    #[test]
    fn validate_rejects_disallowed_repository() {
        let row = sample_row();
        let profiles = vec!["issue-validator".to_string()];
        let models = vec!["gpt-5".to_string()];
        assert_eq!(
            validate_row(&row, &["other/repo".to_string()], &profiles, &models).unwrap_err(),
            ValidationError::RepositoryNotAllowed(row.repository.clone())
        );
    }

    #[test]
    fn validate_rejects_disallowed_profile() {
        let row = sample_row();
        let repos = vec![row.repository.clone()];
        let models = vec!["gpt-5".to_string()];
        assert_eq!(
            validate_row(&row, &repos, &["other-profile".to_string()], &models).unwrap_err(),
            ValidationError::ProfileNotAllowed(row.agent_profile.clone())
        );
    }

    #[test]
    fn validate_rejects_disallowed_requested_model() {
        let row = sample_row();
        let repos = vec![row.repository.clone()];
        let profiles = vec![row.agent_profile.clone()];
        assert_eq!(
            validate_row(&row, &repos, &profiles, &["other-model".to_string()]).unwrap_err(),
            ValidationError::RequestedModelNotAllowed(row.requested_model.clone())
        );
    }

    #[test]
    fn validate_rejects_disallowed_fallback_model() {
        let row = sample_row();
        let repos = vec![row.repository.clone()];
        let profiles = vec![row.agent_profile.clone()];
        let models = vec![row.requested_model.clone()];
        assert_eq!(
            validate_row(&row, &repos, &profiles, &models).unwrap_err(),
            ValidationError::FallbackModelNotAllowed(row.fallback_model.clone().unwrap())
        );
    }

    #[test]
    fn validate_allows_missing_fallback_model() {
        let mut row = sample_row();
        row.fallback_model = None;
        let repos = vec![row.repository.clone()];
        let profiles = vec![row.agent_profile.clone()];
        let models = vec![row.requested_model.clone()];
        assert!(validate_row(&row, &repos, &profiles, &models).is_ok());
    }
}
