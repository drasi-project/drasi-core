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

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

pub const ROUTE_QUERY_ID: &str = "route-awaiting-workgraph-items";

fn default_github_graphql_url() -> String {
    "https://api.github.com/graphql".to_string()
}

fn default_github_rest_url() -> String {
    "https://api.github.com".to_string()
}

fn default_github_token_env() -> String {
    "GITHUB_TOKEN".to_string()
}

fn default_timeout_secs() -> u64 {
    30
}

fn default_strict_recovery() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::workgraph_router::StatusTransition)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct StatusTransition {
    pub from: String,
    pub to: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct WorkgraphRouterReactionConfig {
    pub policy_id: String,
    pub policy_type: String,
    pub policy_version: String,
    pub allowed_projects: Vec<String>,
    pub allowed_repos: Vec<String>,
    pub allowed_event_types: Vec<String>,
    pub allowed_status_transitions: Vec<StatusTransition>,
    pub allowed_responsibility_types: Vec<String>,
    pub allowed_actors: Vec<String>,
    pub trusted_routing_authors: Vec<String>,
    pub trusted_launcher_authors: Vec<String>,
    pub trusted_agent_authors: Vec<String>,
    pub trusted_router_authors: Vec<String>,
    #[serde(default = "default_github_graphql_url")]
    pub github_graphql_url: String,
    #[serde(default = "default_github_rest_url")]
    pub github_rest_url: String,
    #[serde(default = "default_github_token_env")]
    pub github_token_env: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_strict_recovery")]
    pub strict_recovery: bool,
}

impl std::fmt::Debug for WorkgraphRouterReactionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkgraphRouterReactionConfig")
            .field("policy_id", &self.policy_id)
            .field("policy_type", &self.policy_type)
            .field("policy_version", &self.policy_version)
            .field("allowed_projects", &self.allowed_projects)
            .field("allowed_repos", &self.allowed_repos)
            .field("allowed_event_types", &self.allowed_event_types)
            .field(
                "allowed_status_transitions",
                &self.allowed_status_transitions,
            )
            .field(
                "allowed_responsibility_types",
                &self.allowed_responsibility_types,
            )
            .field("allowed_actors", &self.allowed_actors)
            .field("trusted_routing_authors", &self.trusted_routing_authors)
            .field("trusted_launcher_authors", &self.trusted_launcher_authors)
            .field("trusted_agent_authors", &self.trusted_agent_authors)
            .field("trusted_router_authors", &self.trusted_router_authors)
            .field("github_graphql_url", &self.github_graphql_url)
            .field("github_rest_url", &self.github_rest_url)
            .field("github_token_env", &self.github_token_env)
            .field("timeout_secs", &self.timeout_secs)
            .field("strict_recovery", &self.strict_recovery)
            .finish()
    }
}

impl Default for WorkgraphRouterReactionConfig {
    fn default() -> Self {
        Self {
            policy_id: String::new(),
            policy_type: "rules_v1".to_string(),
            policy_version: "1.0.0".to_string(),
            allowed_projects: Vec::new(),
            allowed_repos: Vec::new(),
            allowed_event_types: vec!["CompletedIssueValidation".to_string()],
            allowed_status_transitions: vec![
                StatusTransition {
                    from: "AwaitingRouting".to_string(),
                    to: "AwaitingIssueRiskProfiling".to_string(),
                },
                StatusTransition {
                    from: "AwaitingRouting".to_string(),
                    to: "NeedsMoreInformation".to_string(),
                },
            ],
            allowed_responsibility_types: vec![
                "issue-validation".to_string(),
                "issue-risk-profiling".to_string(),
                "issue-correction".to_string(),
            ],
            allowed_actors: Vec::new(),
            trusted_routing_authors: Vec::new(),
            trusted_launcher_authors: Vec::new(),
            trusted_agent_authors: Vec::new(),
            trusted_router_authors: Vec::new(),
            github_graphql_url: default_github_graphql_url(),
            github_rest_url: default_github_rest_url(),
            github_token_env: default_github_token_env(),
            timeout_secs: default_timeout_secs(),
            strict_recovery: default_strict_recovery(),
        }
    }
}

impl WorkgraphRouterReactionConfig {
    pub fn validate(
        &self,
        query_ids: &[String],
        priority_queue_capacity: Option<usize>,
    ) -> anyhow::Result<()> {
        if query_ids.len() != 1 {
            anyhow::bail!(
                "workgraph-router requires exactly one query subscription; got {}",
                query_ids.len()
            );
        }
        if query_ids[0] != ROUTE_QUERY_ID {
            anyhow::bail!(
                "workgraph-router must subscribe to '{}'; got '{}'",
                ROUTE_QUERY_ID,
                query_ids[0]
            );
        }
        if self.policy_id.trim().is_empty() {
            anyhow::bail!("policyId is required");
        }
        if self.policy_type.trim().is_empty() {
            anyhow::bail!("policyType is required");
        }
        if self.policy_version.trim().is_empty() {
            anyhow::bail!("policyVersion is required");
        }
        if self.timeout_secs == 0 {
            anyhow::bail!("timeoutSecs must be greater than 0");
        }
        if matches!(priority_queue_capacity, Some(0)) {
            anyhow::bail!("priorityQueueCapacity must be greater than 0");
        }
        if self.allowed_status_transitions.is_empty() {
            anyhow::bail!("allowedStatusTransitions must contain at least one transition");
        }
        if self.allowed_event_types.is_empty() {
            anyhow::bail!("allowedEventTypes must contain at least one entry");
        }
        if self.github_graphql_url.trim().is_empty() || self.github_rest_url.trim().is_empty() {
            anyhow::bail!("githubGraphqlUrl and githubRestUrl are required");
        }
        if self.github_token_env.trim().is_empty() {
            anyhow::bail!("githubTokenEnv is required");
        }
        Ok(())
    }

    pub fn allows_transition(&self, from: &str, to: &str) -> bool {
        self.allowed_status_transitions
            .iter()
            .any(|t| t.from == from && t.to == to)
    }

    pub fn trusted_observed_authors(&self) -> HashSet<String> {
        self.trusted_routing_authors
            .iter()
            .chain(self.trusted_launcher_authors.iter())
            .chain(self.trusted_agent_authors.iter())
            .chain(self.trusted_router_authors.iter())
            .cloned()
            .collect()
    }
}
