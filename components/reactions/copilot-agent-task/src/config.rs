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

use anyhow::bail;
use drasi_workgraph_common::trust::{validate_trusted_author, ActorType, TrustedAuthor};
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

/// Settings for the single `ExecutionStarted` WorkGraphEvent/v1 comment posted
/// per successful launch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CommentApiConfig {
    /// Number of authoritative reconciliation reads after an ambiguous task or
    /// comment write before failing stopped. Ambiguous writes are never retried.
    #[serde(default = "CommentApiConfig::default_max_attempts")]
    pub max_attempts: u32,
    /// Backoff between authoritative reconciliation reads.
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

    /// Numeric GitHub database ID of the identity whose
    /// `ResponsibilityAssigned` comments this reaction trusts (the identity the
    /// assigning HTTP reaction posts as).
    ///
    /// Together with [`Self::trusted_assignment_author_type`] this is the whole
    /// trust key for the assignment. Node IDs are audit data on the observed
    /// author and are never configured; logins are never used for trust
    /// because they can be renamed and reclaimed; no GitHub App attribution is
    /// involved. See [`drasi_workgraph_common::trust`].
    pub trusted_assignment_author_database_id: u64,

    /// Actor type of the identity whose `ResponsibilityAssigned` comments this
    /// reaction trusts.
    #[serde(default = "CopilotAgentTaskReactionConfig::default_actor_type")]
    pub trusted_assignment_author_type: ActorType,

    /// Numeric GitHub database ID of the identity **this** reaction posts as.
    ///
    /// Used only to adopt its own `ExecutionStarted` comment after an ambiguous
    /// write. It must name the same account as `token` authenticates as (and as
    /// `expectedGithubUserId`, when that preflight is configured).
    pub trusted_execution_author_database_id: u64,

    /// Actor type of the identity this reaction posts as.
    #[serde(default = "CopilotAgentTaskReactionConfig::default_actor_type")]
    pub trusted_execution_author_type: ActorType,

    /// Immutable node ID (`PVTSSF_...`) of the Project single-select status
    /// field. Preflight requires the item's status field to be exactly this
    /// node and its value to be `AwaitingValidation`.
    pub expected_project_status_field_node_id: String,

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

    fn default_actor_type() -> ActorType {
        ActorType::Bot
    }

    /// The trusted author of the `ResponsibilityAssigned` assignment.
    pub fn trusted_assignment_author(&self) -> TrustedAuthor {
        TrustedAuthor::new(
            self.trusted_assignment_author_database_id,
            self.trusted_assignment_author_type,
        )
    }

    /// The trusted author of this reaction's own `ExecutionStarted` comment.
    pub fn trusted_execution_author(&self) -> TrustedAuthor {
        TrustedAuthor::new(
            self.trusted_execution_author_database_id,
            self.trusted_execution_author_type,
        )
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
            trusted_assignment_author_database_id: 0,
            trusted_assignment_author_type: Self::default_actor_type(),
            trusted_execution_author_database_id: 0,
            trusted_execution_author_type: Self::default_actor_type(),
            expected_project_status_field_node_id: String::new(),
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
            .field(
                "trusted_assignment_author_database_id",
                &self.trusted_assignment_author_database_id,
            )
            .field(
                "trusted_assignment_author_type",
                &self.trusted_assignment_author_type,
            )
            .field(
                "trusted_execution_author_database_id",
                &self.trusted_execution_author_database_id,
            )
            .field(
                "trusted_execution_author_type",
                &self.trusted_execution_author_type,
            )
            .field(
                "expected_project_status_field_node_id",
                &self.expected_project_status_field_node_id,
            )
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
        validate_trusted_author(
            "trustedAssignmentAuthorDatabaseId",
            &self.trusted_assignment_author(),
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        validate_trusted_author(
            "trustedExecutionAuthorDatabaseId",
            &self.trusted_execution_author(),
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        // Both name this reaction's own account, so a disagreement is a
        // misconfiguration that would make the reaction unable to adopt its own
        // `ExecutionStarted` comment — and therefore post a duplicate one.
        if let Some(expected) = self.expected_github_user_id.as_deref() {
            if expected != self.trusted_execution_author_database_id.to_string() {
                bail!(
                    "`expectedGithubUserId` ({expected}) must be the same account as \
                     `trustedExecutionAuthorDatabaseId` ({})",
                    self.trusted_execution_author_database_id
                );
            }
        }
        if !self
            .expected_project_status_field_node_id
            .starts_with("PVTSSF_")
        {
            bail!(
                "`expectedProjectStatusFieldNodeId` must be a Projects v2 single-select field node ID starting with 'PVTSSF_'"
            );
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
        validate_endpoint("githubApiBaseUrl", &self.github_api_base_url)?;
        validate_endpoint("githubGraphqlUrl", &self.github_graphql_url)?;
        Ok(())
    }
}

/// Validate an HTTP(S) endpoint: it must parse, must not embed credentials, and
/// must use `https` (plain `http` is allowed only for loopback test servers).
fn validate_endpoint(field: &str, value: &str) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(value)
        .map_err(|error| anyhow::anyhow!("{field} '{value}' is not a valid URL: {error}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("{field} must not embed credentials");
    }
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    match url.scheme() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        scheme => anyhow::bail!(
            "{field} scheme '{scheme}' is not allowed; use https (http is permitted only for loopback test endpoints)"
        ),
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
            trusted_assignment_author_database_id: 4021243,
            trusted_assignment_author_type: ActorType::Bot,
            trusted_execution_author_database_id: 90210,
            trusted_execution_author_type: ActorType::Bot,
            expected_project_status_field_node_id: "PVTSSF_status".to_string(),
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
        config.expected_github_user_id = Some("90210".to_string());
        assert!(config.validate(&["query-1".to_string()]).is_ok());
    }

    #[test]
    fn the_token_owner_guard_must_name_the_execution_author() {
        // Both describe this reaction's own account; if they disagreed, the
        // reaction could never adopt its own ExecutionStarted comment and would
        // post a duplicate instead.
        let mut config = valid_config();
        config.expected_github_user_id = Some("4021243".to_string());
        let error = config
            .validate(&["query-1".to_string()])
            .expect_err("mismatched token owner");
        assert!(
            error
                .to_string()
                .contains("trustedExecutionAuthorDatabaseId"),
            "{error}"
        );
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
    fn requires_an_authoritative_database_id_for_both_roles() {
        let mut cfg = valid_config();
        cfg.trusted_assignment_author_database_id = 0;
        let error = cfg
            .validate(&["q".to_string()])
            .expect_err("no assignment author");
        assert!(error
            .to_string()
            .contains("trustedAssignmentAuthorDatabaseId"));

        let mut cfg = valid_config();
        cfg.trusted_execution_author_database_id = 0;
        let error = cfg
            .validate(&["q".to_string()])
            .expect_err("no execution author");
        assert!(error
            .to_string()
            .contains("trustedExecutionAuthorDatabaseId"));
    }

    #[test]
    fn each_role_keeps_its_own_database_id_and_actor_type() {
        let cfg = valid_config();
        assert_eq!(
            cfg.trusted_assignment_author(),
            TrustedAuthor::new(4021243, ActorType::Bot)
        );
        assert_eq!(
            cfg.trusted_execution_author(),
            TrustedAuthor::new(90210, ActorType::Bot)
        );

        let mut cfg = valid_config();
        cfg.trusted_execution_author_type = ActorType::User;
        assert_eq!(
            cfg.trusted_execution_author(),
            TrustedAuthor::new(90210, ActorType::User)
        );
        assert_eq!(
            cfg.trusted_assignment_author(),
            TrustedAuthor::new(4021243, ActorType::Bot),
            "the roles must not share a trust value"
        );
    }

    #[test]
    fn actor_types_default_to_bot_and_no_node_id_is_configurable() {
        let cfg: CopilotAgentTaskReactionConfig = serde_json::from_value(serde_json::json!({
            "token": "ghp_test",
            "allowedRepositories": ["o/r"],
            "allowedProfiles": ["issue-validator"],
            "allowedModels": ["gpt-5"],
            "trustedAssignmentAuthorDatabaseId": 4021243,
            "trustedExecutionAuthorDatabaseId": 90210,
            "expectedProjectStatusFieldNodeId": "PVTSSF_status"
        }))
        .expect("actor types default");
        assert_eq!(cfg.trusted_assignment_author_type, ActorType::Bot);
        assert_eq!(cfg.trusted_execution_author_type, ActorType::Bot);
        cfg.validate(&["q".to_string()]).expect("valid");

        for removed in ["trustedAssignmentAuthors", "trustedAssignmentAuthorNodeId"] {
            let mut json = serde_json::json!({
                "token": "ghp_test",
                "allowedRepositories": ["o/r"],
                "allowedProfiles": ["issue-validator"],
                "allowedModels": ["gpt-5"],
                "trustedAssignmentAuthorDatabaseId": 4021243,
                "trustedExecutionAuthorDatabaseId": 90210,
                "expectedProjectStatusFieldNodeId": "PVTSSF_status"
            });
            json[removed] = serde_json::json!("x");
            let error = serde_json::from_value::<CopilotAgentTaskReactionConfig>(json)
                .expect_err("removed trust field must be rejected");
            assert!(error.to_string().contains(removed), "{error}");
        }
    }

    #[test]
    fn rejects_bad_status_field_node_id() {
        let mut cfg = valid_config();
        cfg.expected_project_status_field_node_id = "PVTF_wrongprefix".to_string();
        let error = cfg
            .validate(&["q".to_string()])
            .expect_err("bad status field node id");
        assert!(error
            .to_string()
            .contains("expectedProjectStatusFieldNodeId"));
    }

    #[test]
    fn rejects_endpoint_with_credentials() {
        let mut cfg = valid_config();
        cfg.github_api_base_url = "https://user:pass@api.github.com".to_string();
        let error = cfg
            .validate(&["q".to_string()])
            .expect_err("credentials in URL");
        assert!(error.to_string().contains("must not embed credentials"));
    }

    #[test]
    fn rejects_non_https_non_loopback_endpoint() {
        let mut cfg = valid_config();
        cfg.github_api_base_url = "http://api.github.com".to_string();
        let error = cfg
            .validate(&["q".to_string()])
            .expect_err("plain http on a public host");
        assert!(error.to_string().contains("is not allowed"));
    }

    #[test]
    fn allows_http_loopback_endpoint() {
        let mut cfg = valid_config();
        cfg.github_api_base_url = "http://127.0.0.1:8080".to_string();
        cfg.github_graphql_url = "http://localhost:8080/graphql".to_string();
        cfg.validate(&["q".to_string()])
            .expect("loopback http is allowed for tests");
    }

    #[test]
    fn debug_redacts_token() {
        let cfg = valid_config();
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("ghp_test"));
        assert!(debug.contains("[REDACTED]"));
    }
}
