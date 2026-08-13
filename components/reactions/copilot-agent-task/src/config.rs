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

//! Configuration for the Copilot Agent Task reaction.

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

/// Default GitHub REST API base URL.
pub fn default_github_api_base_url() -> String {
    "https://api.github.com".to_string()
}

/// Default GitHub GraphQL endpoint.
pub fn default_github_graphql_url() -> String {
    "https://api.github.com/graphql".to_string()
}

/// The Agent Tasks API version this reaction was built and tested against.
/// Sent as the `X-GitHub-Api-Version` header on every REST/Agent Tasks call.
pub const AGENT_TASKS_API_VERSION: &str = "2026-03-10";

pub fn default_agent_tasks_api_version() -> String {
    AGENT_TASKS_API_VERSION.to_string()
}

pub fn default_request_timeout_ms() -> u64 {
    30_000
}

/// Settings for the single `workgraph.execution/v1` comment posted per
/// successful launch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CommentApiConfig {
    /// Number of GraphQL comment-post attempts within a single processing
    /// pass before treating the failure as transient and stopping (the next
    /// restart resumes at the comment step — the task is never recreated).
    #[serde(default = "CommentApiConfig::default_max_attempts")]
    pub max_attempts: u32,
    /// Backoff between in-process retry attempts.
    #[serde(default = "CommentApiConfig::default_retry_backoff_ms")]
    pub retry_backoff_ms: u64,
}

impl CommentApiConfig {
    fn default_max_attempts() -> u32 {
        3
    }
    fn default_retry_backoff_ms() -> u64 {
        500
    }
}

impl Default for CommentApiConfig {
    fn default() -> Self {
        Self {
            max_attempts: Self::default_max_attempts(),
            retry_backoff_ms: Self::default_retry_backoff_ms(),
        }
    }
}

/// Full configuration for the Copilot Agent Task reaction.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CopilotAgentTaskReactionConfig {
    /// GitHub REST API base URL (e.g. `https://api.github.com`, or
    /// `https://GHE_HOST/api/v3` for GitHub Enterprise Server).
    #[serde(default = "default_github_api_base_url")]
    pub github_api_base_url: String,

    /// GitHub GraphQL endpoint.
    #[serde(default = "default_github_graphql_url")]
    pub github_graphql_url: String,

    /// `X-GitHub-Api-Version` sent on Agent Tasks calls.
    #[serde(default = "default_agent_tasks_api_version")]
    pub agent_tasks_api_version: String,

    /// A fine-grained personal access token or GitHub App user-to-server
    /// token with permission to create Agent Tasks, read issues/projects,
    /// and comment on issues in `allowed_repositories`. **Never logged.**
    ///
    /// Resolved from the environment (or a secret store) via `ConfigValue`
    /// at the declarative-config boundary; see
    /// [`crate::descriptor::CopilotAgentTaskReactionConfigDto`].
    pub token: String,

    /// Optional numeric GitHub user ID expected for `token`. When configured,
    /// startup calls `GET /user` and fails closed if the authenticated
    /// identity differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_github_user_id: Option<String>,

    /// Allowlist of `"owner/repo"` strings. A launch row for any other
    /// repository is rejected (fail-closed). Must be non-empty.
    pub allowed_repositories: Vec<String>,

    /// Allowlist of `agentProfile` identifiers. Must be non-empty.
    pub allowed_profiles: Vec<String>,

    /// Allowlist of model identifiers valid for both `requestedModel` and
    /// `fallbackModel`. Must be non-empty.
    pub allowed_models: Vec<String>,

    /// Per-request HTTP timeout in milliseconds.
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,

    /// Settings for the workgraph execution issue comment.
    #[serde(default)]
    pub comment_api: CommentApiConfig,

    /// Always `true` for this reaction — surfaced explicitly in the config
    /// (rather than silently assumed) because ambiguous or ill-understood
    /// launch outcomes must never be silently skipped: a sustained failure
    /// stops the reaction for manual/automatic reconciliation rather than
    /// dropping the launch. This field exists so the intent is visible in
    /// persisted configuration; setting it to `false` is rejected by
    /// [`CopilotAgentTaskReactionConfig::validate`].
    #[serde(default = "CopilotAgentTaskReactionConfig::default_strict_recovery")]
    pub strict_recovery: bool,
}

impl CopilotAgentTaskReactionConfig {
    fn default_strict_recovery() -> bool {
        true
    }
}

impl Default for CopilotAgentTaskReactionConfig {
    fn default() -> Self {
        Self {
            github_api_base_url: default_github_api_base_url(),
            github_graphql_url: default_github_graphql_url(),
            agent_tasks_api_version: default_agent_tasks_api_version(),
            token: String::new(),
            expected_github_user_id: None,
            allowed_repositories: Vec::new(),
            allowed_profiles: Vec::new(),
            allowed_models: Vec::new(),
            request_timeout_ms: default_request_timeout_ms(),
            comment_api: CommentApiConfig::default(),
            strict_recovery: Self::default_strict_recovery(),
        }
    }
}

impl std::fmt::Debug for CopilotAgentTaskReactionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopilotAgentTaskReactionConfig")
            .field("github_api_base_url", &self.github_api_base_url)
            .field("github_graphql_url", &self.github_graphql_url)
            .field("agent_tasks_api_version", &self.agent_tasks_api_version)
            .field("token", &"[REDACTED]")
            .field("expected_github_user_id", &self.expected_github_user_id)
            .field("allowed_repositories", &self.allowed_repositories)
            .field("allowed_profiles", &self.allowed_profiles)
            .field("allowed_models", &self.allowed_models)
            .field("request_timeout_ms", &self.request_timeout_ms)
            .field("comment_api", &self.comment_api)
            .field("strict_recovery", &self.strict_recovery)
            .finish()
    }
}

impl CopilotAgentTaskReactionConfig {
    /// Validate configuration, failing fast (at construction) rather than at
    /// dispatch. Fails closed: empty allowlists or a missing token are
    /// rejected rather than defaulting to "allow everything".
    pub fn validate(&self, query_ids: &[String]) -> anyhow::Result<()> {
        if query_ids.len() != 1 {
            bail!(
                "the Copilot Agent Task reaction must subscribe to exactly one launch query, got {}",
                query_ids.len()
            );
        }
        if self.token.trim().is_empty() {
            bail!("`token` must not be empty (a fine-grained PAT or GitHub App user token is required)");
        }
        if self
            .expected_github_user_id
            .as_deref()
            .is_some_and(|id| id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()))
        {
            bail!("`expectedGithubUserId` must contain only decimal digits");
        }
        if self.allowed_repositories.is_empty() {
            bail!("`allowedRepositories` must not be empty (fail-closed: nothing is allowed by default)");
        }
        if self.allowed_profiles.is_empty() {
            bail!(
                "`allowedProfiles` must not be empty (fail-closed: nothing is allowed by default)"
            );
        }
        if self.allowed_models.is_empty() {
            bail!("`allowedModels` must not be empty (fail-closed: nothing is allowed by default)");
        }
        if self.request_timeout_ms == 0 {
            bail!("`requestTimeoutMs` must be greater than 0");
        }
        if self.comment_api.max_attempts == 0 {
            bail!("`commentApi.maxAttempts` must be greater than 0");
        }
        if !self.strict_recovery {
            bail!(
                "`strictRecovery` must be true for this reaction: ambiguous or failed launches \
                 require reconciliation, never a silent skip"
            );
        }
        reqwest::Url::parse(&self.github_api_base_url)
            .with_context(|| format!("invalid `githubApiBaseUrl`: {}", self.github_api_base_url))?;
        reqwest::Url::parse(&self.github_graphql_url)
            .with_context(|| format!("invalid `githubGraphqlUrl`: {}", self.github_graphql_url))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> CopilotAgentTaskReactionConfig {
        CopilotAgentTaskReactionConfig {
            token: "ghp_test".to_string(),
            allowed_repositories: vec!["drasi-project/drasi-core".to_string()],
            allowed_profiles: vec!["issue-validator".to_string()],
            allowed_models: vec!["gpt-5".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn valid_config_passes() {
        assert!(valid_config().validate(&["query-1".to_string()]).is_ok());
    }

    #[test]
    fn accepts_numeric_expected_github_user_id() {
        let mut config = valid_config();
        config.expected_github_user_id = Some("4021243".to_string());
        assert!(config.validate(&["query-1".to_string()]).is_ok());
    }

    #[test]
    fn rejects_non_numeric_expected_github_user_id() {
        let mut config = valid_config();
        config.expected_github_user_id = Some("trusted-user".to_string());
        let error = config
            .validate(&["query-1".to_string()])
            .expect_err("non-numeric ID must fail");
        assert!(error.to_string().contains("expectedGithubUserId"));
    }

    #[test]
    fn requires_exactly_one_query() {
        assert!(valid_config().validate(&[]).is_err());
        assert!(valid_config()
            .validate(&["q1".to_string(), "q2".to_string()])
            .is_err());
    }

    #[test]
    fn requires_non_empty_token() {
        let mut cfg = valid_config();
        cfg.token = String::new();
        assert!(cfg.validate(&["q".to_string()]).is_err());
    }

    #[test]
    fn requires_non_empty_allowlists() {
        let mut cfg = valid_config();
        cfg.allowed_repositories = vec![];
        assert!(cfg.validate(&["q".to_string()]).is_err());

        let mut cfg = valid_config();
        cfg.allowed_profiles = vec![];
        assert!(cfg.validate(&["q".to_string()]).is_err());

        let mut cfg = valid_config();
        cfg.allowed_models = vec![];
        assert!(cfg.validate(&["q".to_string()]).is_err());
    }

    #[test]
    fn rejects_non_strict_recovery() {
        let mut cfg = valid_config();
        cfg.strict_recovery = false;
        assert!(cfg.validate(&["q".to_string()]).is_err());
    }

    #[test]
    fn rejects_invalid_urls() {
        let mut cfg = valid_config();
        cfg.github_api_base_url = "not a url".to_string();
        assert!(cfg.validate(&["q".to_string()]).is_err());
    }

    #[test]
    fn debug_redacts_token() {
        let cfg = valid_config();
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("ghp_test"));
        assert!(debug.contains("[REDACTED]"));
    }
}
