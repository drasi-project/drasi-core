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

//! Configuration for the WorkGraph router reaction.
//!
//! The reaction can do exactly two things: post one issue comment and set one
//! Project single-select status option, on repositories and Projects an
//! operator has explicitly allowlisted. There is no configurable endpoint,
//! request template, GraphQL document, or status-transition table — the only
//! transitions that exist are the two the event contract defines:
//!
//! ```text
//! AwaitingValidation -> AwaitingIssueRiskProfiling   (validation passed)
//! AwaitingValidation -> NeedsMoreInformation         (validation failed)
//! ```
//!
//! Because those are fixed by
//! [`drasi_workgraph_common::event::RoutingDecidedPayload`], configuration
//! cannot introduce a new destination, a new responsibility, or an intermediate
//! `AwaitingRouting` state.

use drasi_workgraph_common::status::AWAITING_VALIDATION;
use drasi_workgraph_common::trust::{validate_trusted_author, ActorType, TrustedAuthor};
use serde::{Deserialize, Serialize};

/// The only status a routing decision moves away from.
pub const ROUTABLE_STATUS: &str = AWAITING_VALIDATION;

/// The only agent profile whose assignment this router will route.
pub const ROUTED_PROFILE: &str = "issue-validator";

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

fn default_expected_profile() -> String {
    ROUTED_PROFILE.to_string()
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

/// Configuration for [`crate::WorkgraphRouterReaction`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkgraphRouterReactionConfig {
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
    /// The agent profile the routed assignment must name.
    #[serde(default = "default_expected_profile")]
    pub expected_profile: String,
    /// Numeric GitHub database ID whose WorkGraph comments this reaction
    /// trusts.
    ///
    /// Together with [`Self::trusted_author_type`] this is the whole trust key.
    /// Node IDs are audit data on the observed author and are never configured;
    /// logins are never used for trust because they can be renamed and
    /// reclaimed; and no GitHub App attribution is involved — the Source does
    /// not expose an authoritative one for comment nodes. The identity this
    /// reaction posts as must be this identity, so it can adopt its own
    /// decision comment after an ambiguous write. See
    /// [`drasi_workgraph_common::trust`] for the contract and the limits of
    /// same-identity attribution.
    pub trusted_author_database_id: u64,
    /// The actor type of the trusted identity.
    #[serde(default = "default_trusted_author_type")]
    pub trusted_author_type: ActorType,
    /// Per-request timeout for GitHub calls.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Must remain `true`; ambiguity is never silently skipped.
    #[serde(default = "default_strict_recovery")]
    pub strict_recovery: bool,
}

impl std::fmt::Debug for WorkgraphRouterReactionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkgraphRouterReactionConfig")
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
            .field("expected_profile", &self.expected_profile)
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

impl Default for WorkgraphRouterReactionConfig {
    fn default() -> Self {
        Self {
            github_rest_url: default_github_rest_url(),
            github_graphql_url: default_github_graphql_url(),
            github_token_env: default_github_token_env(),
            allowed_repositories: Vec::new(),
            allowed_projects: Vec::new(),
            project_status_field_name: default_project_status_field_name(),
            expected_project_status_field_node_id: String::new(),
            expected_profile: default_expected_profile(),
            trusted_author_database_id: 0,
            trusted_author_type: default_trusted_author_type(),
            timeout_secs: default_timeout_secs(),
            strict_recovery: default_strict_recovery(),
        }
    }
}

impl WorkgraphRouterReactionConfig {
    /// Validate the configuration, failing closed on anything permissive.
    pub fn validate(&self, query_ids: &[String]) -> anyhow::Result<()> {
        if query_ids.len() != 1 {
            anyhow::bail!(
                "workgraph-router requires exactly one query subscription; got {}",
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
        if self.expected_profile != ROUTED_PROFILE {
            anyhow::bail!(
                "expectedProfile must be '{ROUTED_PROFILE}' for workgraph.event/v1 routing"
            );
        }
        validate_trusted_author("trustedAuthorDatabaseId", &self.trusted_author())
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        if self.timeout_secs == 0 {
            anyhow::bail!("timeoutSecs must be greater than 0");
        }
        if !self.strict_recovery {
            anyhow::bail!("strictRecovery must remain true for workgraph-router");
        }
        Ok(())
    }

    /// The configured trusted author: numeric database ID + actor type.
    pub fn trusted_author(&self) -> TrustedAuthor {
        TrustedAuthor::new(self.trusted_author_database_id, self.trusted_author_type)
    }

    /// Whether a repository is allowlisted.
    pub fn allows_repository(&self, repository: &str) -> bool {
        self.allowed_repositories
            .iter()
            .any(|allowed| allowed == repository)
    }

    /// Whether a Project node ID is allowlisted.
    pub fn allows_project(&self, project_node_id: &str) -> bool {
        self.allowed_projects
            .iter()
            .any(|allowed| allowed == project_node_id)
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

    fn valid() -> WorkgraphRouterReactionConfig {
        WorkgraphRouterReactionConfig {
            allowed_repositories: vec!["drasi-project/drasi-core".to_string()],
            allowed_projects: vec!["PVT_project".to_string()],
            expected_project_status_field_node_id: "PVTSSF_status".to_string(),
            trusted_author_database_id: 4021243,
            trusted_author_type: ActorType::Bot,
            ..WorkgraphRouterReactionConfig::default()
        }
    }

    fn queries() -> Vec<String> {
        vec!["route-workgraph-items".to_string()]
    }

    #[test]
    fn valid_config_passes() {
        valid().validate(&queries()).expect("valid config");
    }

    #[test]
    fn requires_exactly_one_query() {
        assert!(valid()
            .validate(&[])
            .expect_err("no query")
            .to_string()
            .contains("exactly one query"));
        assert!(valid()
            .validate(&["a".to_string(), "b".to_string()])
            .expect_err("two queries")
            .to_string()
            .contains("exactly one query"));
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
    fn the_trust_key_is_the_database_id_and_actor_type_only() {
        assert_eq!(
            valid().trusted_author(),
            TrustedAuthor::new(4021243, ActorType::Bot)
        );

        let mut config = valid();
        config.trusted_author_type = ActorType::User;
        assert_eq!(
            config.trusted_author(),
            TrustedAuthor::new(4021243, ActorType::User)
        );

        // The actor type defaults, and no node ID is configurable.
        let config: WorkgraphRouterReactionConfig = serde_json::from_value(serde_json::json!({
            "allowedRepositories": ["o/r"],
            "allowedProjects": ["PVT_1"],
            "expectedProjectStatusFieldNodeId": "PVTSSF_1",
            "trustedAuthorDatabaseId": 4021243
        }))
        .expect("actorType defaults");
        assert_eq!(config.trusted_author_type, ActorType::Bot);

        for removed in ["trustedAuthorNodeId", "trustedAuthors"] {
            let mut json = serde_json::json!({
                "allowedRepositories": ["o/r"],
                "allowedProjects": ["PVT_1"],
                "expectedProjectStatusFieldNodeId": "PVTSSF_1",
                "trustedAuthorDatabaseId": 4021243,
                "trustedAuthorType": "Bot"
            });
            json[removed] = serde_json::json!("x");
            let error = serde_json::from_value::<WorkgraphRouterReactionConfig>(json)
                .expect_err("removed trust field must be rejected");
            assert!(error.to_string().contains(removed), "{error}");
        }
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
        config.github_graphql_url = format!(
            "{}://{}:{}@api.github.com/graphql",
            "https", "user", "token"
        );
        assert!(config
            .validate(&queries())
            .expect_err("credentials in URL")
            .to_string()
            .contains("credentials"));
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
    fn only_the_issue_validation_profile_can_be_routed() {
        let mut config = valid();
        config.expected_profile = "issue-risk-profiler".to_string();
        assert!(config
            .validate(&queries())
            .expect_err("foreign profile")
            .to_string()
            .contains("expectedProfile"));
    }

    #[test]
    fn removed_policy_and_transition_knobs_are_rejected() {
        // The routing table is fixed by the event contract; configuration can
        // no longer widen it, name an actor, or introduce AwaitingRouting.
        for (removed, value) in [
            ("policyId", serde_json::json!("policy-1")),
            (
                "allowedStatusTransitions",
                serde_json::json!([{ "from": "AwaitingRouting", "to": "Done" }]),
            ),
            ("allowedActors", serde_json::json!(["mallory"])),
            ("allowedEventTypes", serde_json::json!(["Anything"])),
            ("reservationLeaseSecs", serde_json::json!(120)),
        ] {
            let mut config = serde_json::json!({
                "allowedRepositories": ["drasi-project/drasi-core"],
                "allowedProjects": ["PVT_project"],
                "expectedProjectStatusFieldNodeId": "PVTSSF_status",
                "trustedAuthorDatabaseId": 4021243,
                "trustedAuthorType": "Bot"
            });
            config[removed] = value;
            let error = serde_json::from_value::<WorkgraphRouterReactionConfig>(config)
                .expect_err("removed knob must be rejected");
            assert!(
                error.to_string().contains(removed),
                "unexpected error for '{removed}': {error}"
            );
        }
    }

    #[test]
    fn the_routable_status_is_the_shared_constant() {
        assert_eq!(ROUTABLE_STATUS, "AwaitingValidation");
    }
}
