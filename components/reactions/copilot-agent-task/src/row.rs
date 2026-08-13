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
/// # Field notes
///
/// * `profile_ref` is `"<agentProfile>@<blobSha>"`. The file path is derived
///   from GitHub's custom-agent convention:
///   `.github/agents/<agentProfile>.agent.md`.
/// * `issue_content_version` is the normalized RFC 3339 instant from the
///   issue's GraphQL `lastEditedAt ?? createdAt`.
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
    pub project_node_id: String,
    pub project_owner: String,
    pub project_number: u64,
    pub subject_type: String,
    pub actor_type: String,
    pub actor_id: String,
    pub route_id: String,
    pub responsibility_id: String,
    /// Normalized `lastEditedAt ?? createdAt` RFC 3339 instant.
    pub issue_content_version: String,
    pub agent_profile: String,
    /// `"<agentProfile>@<blobSha>"`.
    pub profile_ref: String,
    pub requested_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_model: Option<String>,
    pub required_event_type: String,
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

    /// Split `profile_ref` into `(agent_profile, expected_blob_sha)`.
    pub fn profile_name_and_sha(&self) -> Result<(&str, &str)> {
        let (profile, sha) = self.profile_ref.rsplit_once('@').with_context(|| {
            format!(
                "profileRef '{}' is not '<agentProfile>@<blobSha>'",
                self.profile_ref
            )
        })?;
        if profile.is_empty() || sha.is_empty() {
            bail!(
                "profileRef '{}' has an empty agent profile or blob SHA",
                self.profile_ref
            );
        }
        if profile != self.agent_profile {
            bail!(
                "profileRef agent profile '{}' does not match agentProfile '{}'",
                profile,
                self.agent_profile
            );
        }
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("profileRef blob SHA must be exactly 40 hexadecimal characters");
        }
        Ok((profile, sha))
    }

    pub fn profile_path_and_sha(&self) -> Result<(String, &str)> {
        let (profile, sha) = self.profile_name_and_sha()?;
        if profile.contains('/') || profile.contains("..") {
            bail!("agentProfile '{profile}' is not safe for a custom-agent path");
        }
        Ok((format!(".github/agents/{profile}.agent.md"), sha))
    }

    /// Normalize an RFC 3339 timestamp to a UTC instant using `Z` and only the
    /// fractional precision needed to represent the instant.
    pub fn normalize_rfc3339_instant(value: &str) -> Result<String> {
        let parsed = Self::parse_rfc3339_instant(value)?;
        Ok(parsed.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
    }

    pub fn parse_rfc3339_instant(value: &str) -> Result<chrono::DateTime<chrono::Utc>> {
        chrono::DateTime::parse_from_rfc3339(value)
            .with_context(|| format!("'{value}' is not an RFC 3339 instant"))
            .map(|parsed| parsed.with_timezone(&chrono::Utc))
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
    if let Err(e) = LaunchRow::parse_rfc3339_instant(&row.issue_content_version) {
        return Err(ValidationError::Malformed(e.to_string()));
    }
    if row.project_number == 0
        || row.project_node_id.trim().is_empty()
        || row.project_owner.trim().is_empty()
        || row.subject_type.trim().is_empty()
        || row.actor_type.trim().is_empty()
        || row.actor_id.trim().is_empty()
    {
        return Err(ValidationError::Malformed(
            "project/subject/actor target fields must be non-empty".to_string(),
        ));
    }
    if row.subject_type != "Issue" {
        return Err(ValidationError::Malformed(
            "subjectType must be 'Issue'".to_string(),
        ));
    }
    if row.actor_type != "Agent" || row.actor_id != row.agent_profile {
        return Err(ValidationError::Malformed(
            "actorType must be 'Agent' and actorId must equal agentProfile".to_string(),
        ));
    }
    if row.required_event_type != "CompletedIssueValidation" {
        return Err(ValidationError::Malformed(
            "requiredEventType must be 'CompletedIssueValidation'".to_string(),
        ));
    }
    if row.expected_project_status != "AwaitingValidation" {
        return Err(ValidationError::Malformed(
            "expectedProjectStatus must be 'AwaitingValidation'".to_string(),
        ));
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
            project_node_id: "PVT_test".to_string(),
            project_owner: "drasi-project".to_string(),
            project_number: 3,
            subject_type: "Issue".to_string(),
            actor_type: "Agent".to_string(),
            actor_id: "issue-validator".to_string(),
            route_id: "route-1".to_string(),
            responsibility_id: "resp-1".to_string(),
            issue_content_version: "2026-08-13T19:00:00Z".to_string(),
            agent_profile: "issue-validator".to_string(),
            profile_ref: "issue-validator@0123456789abcdef0123456789abcdef01234567".to_string(),
            requested_model: "gpt-5".to_string(),
            fallback_model: Some("gpt-4".to_string()),
            required_event_type: "CompletedIssueValidation".to_string(),
            base_ref: "main".to_string(),
            expected_project_status: "AwaitingValidation".to_string(),
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
    fn profile_ref_derives_custom_agent_path() {
        let row = sample_row();
        let (path, sha) = row.profile_path_and_sha().unwrap();
        assert_eq!(path, ".github/agents/issue-validator.agent.md");
        assert_eq!(sha, "0123456789abcdef0123456789abcdef01234567");
    }

    #[test]
    fn profile_ref_without_at_is_rejected() {
        let mut row = sample_row();
        row.profile_ref = "no-at-sign".to_string();
        assert!(row.profile_path_and_sha().is_err());
    }

    #[test]
    fn profile_ref_must_match_agent_profile() {
        let mut row = sample_row();
        row.profile_ref = "other@0123456789abcdef0123456789abcdef01234567".to_string();
        assert!(row.profile_path_and_sha().is_err());
    }

    #[test]
    fn normalizes_content_version_to_utc() {
        assert_eq!(
            LaunchRow::normalize_rfc3339_instant("2026-08-13T12:00:00-07:00").unwrap(),
            "2026-08-13T19:00:00Z"
        );
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
