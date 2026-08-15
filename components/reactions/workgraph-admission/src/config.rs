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

//! Configuration for the WorkGraph admission reaction.
//!
//! The reaction is deliberately narrow: it can post one issue comment and set
//! one Project single-select status field, on repositories and Projects that an
//! operator has explicitly allowlisted. Nothing in this configuration can widen
//! it into a general-purpose GitHub client — there is no request template, no
//! arbitrary GraphQL document, and no configurable mutation.

use drasi_workgraph_common::trust::{validate_trusted_author, ActorType, TrustedAuthor};
use serde::{Deserialize, Serialize};

/// The status every admitted Project Item is moved to.
pub const ADMITTED_STATUS: &str = drasi_workgraph_common::status::AWAITING_VALIDATION;

/// The only responsibility this reaction can assign.
pub const ADMISSION_PROFILE: &str = "issue-validator";

fn default_github_rest_url() -> String {
    "https://api.github.com".to_string()
}

fn default_github_graphql_url() -> String {
    "https://api.github.com/graphql".to_string()
}

fn default_github_token_env() -> String {
    "GITHUB_TOKEN".to_string()
}

fn default_project_status_field_name() -> String {
    "Status".to_string()
}

fn default_agent_profile() -> String {
    ADMISSION_PROFILE.to_string()
}

fn default_profile_base_ref() -> String {
    "main".to_string()
}

fn default_timeout_secs() -> u64 {
    30
}

fn default_strict_recovery() -> bool {
    true
}

fn default_trusted_author_type() -> ActorType {
    ActorType::Bot
}

/// Configuration for [`crate::WorkgraphAdmissionReaction`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkgraphAdmissionReactionConfig {
    /// GitHub REST base URL.
    #[serde(default = "default_github_rest_url")]
    pub github_rest_url: String,
    /// GitHub GraphQL endpoint.
    #[serde(default = "default_github_graphql_url")]
    pub github_graphql_url: String,
    /// Environment variable holding the GitHub token.
    #[serde(default = "default_github_token_env")]
    pub github_token_env: String,
    /// Repositories (`owner/repo`) this reaction may write to. Empty allows nothing.
    pub allowed_repositories: Vec<String>,
    /// Project (v2) node IDs this reaction may mutate. Empty allows nothing.
    pub allowed_projects: Vec<String>,
    /// Name of the Project single-select status field.
    #[serde(default = "default_project_status_field_name")]
    pub project_status_field_name: String,
    /// Immutable node ID (`PVTSSF_...`) of that status field.
    pub expected_project_status_field_node_id: String,
    /// The status an eligible Project Item must currently hold.
    pub expected_source_status: String,
    /// The agent profile assigned by the responsibility.
    #[serde(default = "default_agent_profile")]
    pub agent_profile: String,
    /// Git ref the agent profile blob is pinned from.
    #[serde(default = "default_profile_base_ref")]
    pub profile_base_ref: String,
    /// The numeric GitHub database ID of the identity this reaction posts as.
    ///
    /// Together with [`Self::trusted_author_type`] this is the **whole** trust
    /// key: it is what lets the reaction adopt a comment it already wrote after
    /// an ambiguous write. No node ID and no GitHub App ID is configured —
    /// logins are never used for trust because they can be renamed and
    /// reclaimed. See [`drasi_workgraph_common::trust`] for the contract and
    /// its limits.
    pub trusted_author_database_id: u64,
    /// The actor type of the identity this reaction posts as.
    #[serde(default = "default_trusted_author_type")]
    pub trusted_author_type: ActorType,
    /// Per-request timeout for GitHub calls.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Must remain `true`; ambiguity is never silently skipped.
    #[serde(default = "default_strict_recovery")]
    pub strict_recovery: bool,
}

impl std::fmt::Debug for WorkgraphAdmissionReactionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkgraphAdmissionReactionConfig")
            .field("github_rest_url", &self.github_rest_url)
            .field("github_graphql_url", &self.github_graphql_url)
            .field("github_token_env", &self.github_token_env)
            .field("allowed_repositories", &self.allowed_repositories)
            .field("allowed_projects", &self.allowed_projects)
            .field("project_status_field_name", &self.project_status_field_name)
            .field(
                "expected_project_status_field_node_id",
                &self.expected_project_status_field_node_id,
            )
            .field("expected_source_status", &self.expected_source_status)
            .field("agent_profile", &self.agent_profile)
            .field("profile_base_ref", &self.profile_base_ref)
            .field(
                "trusted_author_database_id",
                &self.trusted_author_database_id,
            )
            .field("trusted_author_type", &self.trusted_author_type)
            .field("timeout_secs", &self.timeout_secs)
            .field("strict_recovery", &self.strict_recovery)
            .finish()
    }
}

impl Default for WorkgraphAdmissionReactionConfig {
    fn default() -> Self {
        Self {
            github_rest_url: default_github_rest_url(),
            github_graphql_url: default_github_graphql_url(),
            github_token_env: default_github_token_env(),
            allowed_repositories: Vec::new(),
            allowed_projects: Vec::new(),
            project_status_field_name: default_project_status_field_name(),
            expected_project_status_field_node_id: String::new(),
            expected_source_status: String::new(),
            agent_profile: default_agent_profile(),
            profile_base_ref: default_profile_base_ref(),
            trusted_author_database_id: 0,
            trusted_author_type: default_trusted_author_type(),
            timeout_secs: default_timeout_secs(),
            strict_recovery: default_strict_recovery(),
        }
    }
}

impl WorkgraphAdmissionReactionConfig {
    /// Validate the configuration, failing closed on anything permissive.
    pub fn validate(&self, query_ids: &[String]) -> anyhow::Result<()> {
        if query_ids.len() != 1 {
            anyhow::bail!(
                "workgraph-admission requires exactly one query subscription; got {}",
                query_ids.len()
            );
        }
        validate_endpoint("githubRestUrl", &self.github_rest_url)?;
        validate_endpoint("githubGraphqlUrl", &self.github_graphql_url)?;
        if self.github_token_env.trim().is_empty() {
            anyhow::bail!("githubTokenEnv is required");
        }
        if self.allowed_repositories.is_empty() {
            anyhow::bail!("allowedRepositories must contain at least one 'owner/repo'");
        }
        for repository in &self.allowed_repositories {
            if repository.split('/').count() != 2
                || repository.split('/').any(|part| part.trim().is_empty())
            {
                anyhow::bail!("allowedRepositories entry '{repository}' must be 'owner/repo'");
            }
        }
        if self.allowed_projects.is_empty() {
            anyhow::bail!("allowedProjects must contain at least one Project node ID");
        }
        for project in &self.allowed_projects {
            if !project.starts_with("PVT_") {
                anyhow::bail!("allowedProjects entry '{project}' must start with 'PVT_'");
            }
        }
        if self.project_status_field_name.trim().is_empty() {
            anyhow::bail!("projectStatusFieldName is required");
        }
        if !self
            .expected_project_status_field_node_id
            .starts_with("PVTSSF_")
        {
            anyhow::bail!("expectedProjectStatusFieldNodeId must start with 'PVTSSF_'");
        }
        if self.expected_source_status.trim().is_empty() {
            anyhow::bail!("expectedSourceStatus is required");
        }
        if self.expected_source_status == ADMITTED_STATUS {
            anyhow::bail!("expectedSourceStatus must differ from '{ADMITTED_STATUS}'");
        }
        if self.agent_profile != ADMISSION_PROFILE {
            anyhow::bail!(
                "agentProfile must be '{ADMISSION_PROFILE}' for workgraph.event/v1 admission"
            );
        }
        if self.profile_base_ref.trim().is_empty() {
            anyhow::bail!("profileBaseRef is required");
        }
        validate_trusted_author("trustedAuthorDatabaseId", &self.trusted_author())
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        if self.timeout_secs == 0 {
            anyhow::bail!("timeoutSecs must be greater than 0");
        }
        if !self.strict_recovery {
            anyhow::bail!("strictRecovery must remain true for workgraph-admission");
        }
        Ok(())
    }

    /// Whether a repository is allowlisted.
    pub fn allows_repository(&self, repository: &str) -> bool {
        self.allowed_repositories
            .iter()
            .any(|allowed| allowed == repository)
    }

    /// The configured trusted author: numeric database ID + actor type.
    pub fn trusted_author(&self) -> TrustedAuthor {
        TrustedAuthor::new(self.trusted_author_database_id, self.trusted_author_type)
    }

    /// Whether a Project node ID is allowlisted.
    pub fn allows_project(&self, project_node_id: &str) -> bool {
        self.allowed_projects
            .iter()
            .any(|allowed| allowed == project_node_id)
    }

    /// The repository path the agent profile blob is pinned from.
    pub fn profile_path(&self) -> String {
        format!(".github/agents/{}.agent.md", self.agent_profile)
    }
}

/// Reject endpoints that are not plaintext-safe.
///
/// Only `https` is accepted, except for loopback hosts so integration tests can
/// point the reaction at a local mock. Credentials embedded in the URL are
/// always rejected: the token belongs in the configured environment variable.
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

    fn valid() -> WorkgraphAdmissionReactionConfig {
        WorkgraphAdmissionReactionConfig {
            allowed_repositories: vec!["drasi-project/drasi-core".to_string()],
            allowed_projects: vec!["PVT_project".to_string()],
            expected_project_status_field_node_id: "PVTSSF_status".to_string(),
            expected_source_status: "Triage".to_string(),
            trusted_author_database_id: 4021243,
            trusted_author_type: ActorType::Bot,
            ..WorkgraphAdmissionReactionConfig::default()
        }
    }

    fn queries() -> Vec<String> {
        vec!["admit-workgraph-items".to_string()]
    }

    #[test]
    fn valid_config_passes() {
        valid().validate(&queries()).expect("valid config");
    }

    #[test]
    fn requires_exactly_one_query() {
        let error = valid().validate(&[]).expect_err("no query");
        assert!(error.to_string().contains("exactly one query"));
        let error = valid()
            .validate(&["a".to_string(), "b".to_string()])
            .expect_err("two queries");
        assert!(error.to_string().contains("exactly one query"));
    }

    #[test]
    fn empty_allowlists_allow_nothing() {
        let mut config = valid();
        config.allowed_repositories.clear();
        assert!(config
            .validate(&queries())
            .expect_err("empty repositories")
            .to_string()
            .contains("allowedRepositories"));

        let mut config = valid();
        config.allowed_projects.clear();
        assert!(config
            .validate(&queries())
            .expect_err("empty projects")
            .to_string()
            .contains("allowedProjects"));
    }

    #[test]
    fn only_https_or_loopback_endpoints_are_accepted() {
        let mut config = valid();
        config.github_rest_url = "http://api.example.com".to_string();
        assert!(config
            .validate(&queries())
            .expect_err("plaintext remote endpoint")
            .to_string()
            .contains("not allowed"));

        let mut config = valid();
        config.github_rest_url = "http://127.0.0.1:8080".to_string();
        config
            .validate(&queries())
            .expect("loopback endpoints are allowed for tests");

        let mut config = valid();
        config.github_graphql_url = "https://user:pass@api.github.com/graphql".to_string();
        assert!(config
            .validate(&queries())
            .expect_err("credentials in URL")
            .to_string()
            .contains("credentials"));

        let mut config = valid();
        config.github_rest_url = "file:///etc/passwd".to_string();
        assert!(config.validate(&queries()).is_err());
    }

    #[test]
    fn source_status_must_differ_from_destination() {
        let mut config = valid();
        config.expected_source_status = ADMITTED_STATUS.to_string();
        assert!(config
            .validate(&queries())
            .expect_err("source equals destination")
            .to_string()
            .contains("must differ"));
    }

    #[test]
    fn an_authoritative_author_database_id_is_required() {
        let mut config = valid();
        config.trusted_author_database_id = 0;
        assert!(config
            .validate(&queries())
            .expect_err("no trusted author")
            .to_string()
            .contains("trustedAuthorDatabaseId"));
    }

    #[test]
    fn the_actor_type_defaults_to_bot_and_is_part_of_the_trust_key() {
        let config: WorkgraphAdmissionReactionConfig = serde_json::from_value(serde_json::json!({
            "allowedRepositories": ["o/r"],
            "allowedProjects": ["PVT_1"],
            "expectedProjectStatusFieldNodeId": "PVTSSF_1",
            "expectedSourceStatus": "Triage",
            "trustedAuthorDatabaseId": 4021243
        }))
        .expect("actorType defaults");
        assert_eq!(config.trusted_author_type, ActorType::Bot);
        assert_eq!(
            config.trusted_author(),
            TrustedAuthor::new(4021243, ActorType::Bot)
        );

        let mut config = valid();
        config.trusted_author_type = ActorType::User;
        assert_eq!(
            config.trusted_author(),
            TrustedAuthor::new(4021243, ActorType::User)
        );
    }

    #[test]
    fn no_node_id_or_author_array_can_be_configured() {
        // The node ID is audit data on the observed author, never a configured
        // trust value.
        for rejected in [
            serde_json::json!({ "trustedAuthorNodeId": "U_kgDOBmvcSA" }),
            serde_json::json!({ "trustedAuthors": [{ "databaseId": 4021243 }] }),
        ] {
            let mut config = serde_json::json!({
                "allowedRepositories": ["o/r"],
                "allowedProjects": ["PVT_1"],
                "expectedProjectStatusFieldNodeId": "PVTSSF_1",
                "expectedSourceStatus": "Triage",
                "trustedAuthorDatabaseId": 4021243,
                "trustedAuthorType": "Bot"
            });
            let (key, value) = rejected
                .as_object()
                .and_then(|map| map.iter().next())
                .map(|(key, value)| (key.clone(), value.clone()))
                .expect("one field");
            config[&key] = value;
            let error = serde_json::from_value::<WorkgraphAdmissionReactionConfig>(config)
                .expect_err("removed trust field must be rejected");
            assert!(error.to_string().contains(&key), "{error}");
        }
    }

    #[test]
    fn strict_recovery_cannot_be_disabled() {
        let mut config = valid();
        config.strict_recovery = false;
        assert!(config
            .validate(&queries())
            .expect_err("strict recovery off")
            .to_string()
            .contains("strictRecovery"));
    }

    #[test]
    fn profile_path_is_derived_from_the_profile_name() {
        assert_eq!(
            valid().profile_path(),
            ".github/agents/issue-validator.agent.md"
        );
    }

    #[test]
    fn unknown_config_fields_are_rejected() {
        let error = serde_json::from_value::<WorkgraphAdmissionReactionConfig>(serde_json::json!({
            "allowedRepositories": ["o/r"],
            "allowedProjects": ["PVT_1"],
            "expectedProjectStatusFieldNodeId": "PVTSSF_1",
            "expectedSourceStatus": "Triage",
            "trustedAuthorDatabaseId": 1,
            "trustedAuthorType": "Bot",
            "arbitraryEndpoint": "https://evil.example"
        }))
        .expect_err("unknown fields rejected");
        assert!(error.to_string().contains("arbitraryEndpoint"));
    }
}
