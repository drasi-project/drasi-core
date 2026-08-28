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

use crate::agents::AgentFileLocation;
use anyhow::{ensure, Result};
use drasi_lib::wal::CapacityPolicy;
use drasi_lib::DurabilityConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

fn default_max_events() -> u64 {
    DurabilityConfig::default().max_events
}

#[derive(Serialize, Deserialize)]
#[serde(
    remote = "DurabilityConfig",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub(crate) struct DurabilityConfigDef {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_max_events")]
    max_events: u64,
    #[serde(default)]
    capacity_policy: CapacityPolicy,
}

pub const DEFAULT_BODY_LIMIT_BYTES: usize = 25 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskIssueType {
    pub id: String,
    pub name: String,
}

impl TaskIssueType {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.trim().is_empty() && self.id.trim() == self.id,
            "taskIssueType.id must be a non-empty GraphQL node ID without surrounding whitespace"
        );
        ensure!(
            !self.name.trim().is_empty() && self.name.trim() == self.name,
            "taskIssueType.name must be a non-empty exact Issue Type name without surrounding whitespace"
        );
        Ok(())
    }

    pub fn matches(&self, issue_type: Option<&Value>) -> bool {
        issue_type.is_some_and(|issue_type| {
            issue_type.get("node_id").and_then(Value::as_str) == Some(self.id.as_str())
                && issue_type.get("name").and_then(Value::as_str) == Some(self.name.as_str())
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryFilter {
    names: BTreeSet<String>,
}

impl RepositoryFilter {
    pub fn new(organization: &str, repositories: &[String]) -> Result<Self> {
        let mut names = BTreeSet::new();
        for (index, entry) in repositories.iter().enumerate() {
            ensure!(
                entry.trim() == entry && !entry.is_empty(),
                "repositories[{index}] must be a non-empty repository name without surrounding whitespace"
            );
            let parts: Vec<_> = entry.split('/').collect();
            let name = match parts.as_slice() {
                [name] => *name,
                [owner, name] => {
                    ensure!(
                        owner.eq_ignore_ascii_case(organization),
                        "repositories[{index}] owner '{owner}' does not match configured organization '{organization}'"
                    );
                    *name
                }
                _ => {
                    anyhow::bail!(
                        "repositories[{index}] must be a repository name or 'organization/name'"
                    )
                }
            };
            ensure!(
                !name.is_empty() && name != "." && name != "..",
                "repositories[{index}] has an invalid repository name"
            );
            ensure!(
                name.len() <= 100
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte)),
                "repositories[{index}] repository name must be at most 100 ASCII letters, digits, '.', '-', or '_'"
            );
            names.insert(name.to_ascii_lowercase());
        }
        Ok(Self { names })
    }

    pub fn includes_all(&self) -> bool {
        self.names.is_empty()
    }

    pub fn includes_name(&self, name: &str) -> bool {
        self.includes_all() || self.names.contains(&name.to_ascii_lowercase())
    }

    pub fn includes_repository(&self, repository: &Value) -> Result<bool> {
        if self.includes_all() {
            return Ok(true);
        }
        let name = repository
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| {
                repository
                    .get("full_name")
                    .and_then(Value::as_str)
                    .and_then(|full_name| full_name.split_once('/').map(|(_, name)| name))
            })
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("repository has no non-empty 'name' or 'full_name'"))?;
        Ok(self.includes_name(name))
    }

    pub fn canonical_names(&self) -> Vec<String> {
        self.names.iter().cloned().collect()
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct WebhookConfig {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub secret: String,
    pub lease_validation_token: String,
    pub body_limit_bytes: usize,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
            path: "/webhook".to_string(),
            secret: String::new(),
            lease_validation_token: String::new(),
            body_limit_bytes: DEFAULT_BODY_LIMIT_BYTES,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustedIdentity {
    /// The exact GitHub GraphQL user node ID.
    pub id: String,
    /// The exact, case-sensitive GitHub login.
    pub login: String,
}

impl TrustedIdentity {
    pub fn validate(&self, field: &str) -> Result<()> {
        ensure!(
            !self.id.trim().is_empty() && self.id.trim() == self.id,
            "{field}.id must be a non-empty GraphQL node ID without surrounding whitespace"
        );
        ensure!(
            !self.login.trim().is_empty() && self.login.trim() == self.login,
            "{field}.login must be a non-empty exact GitHub login without surrounding whitespace"
        );
        Ok(())
    }

    /// Both the node ID and the login must match, mirroring how
    /// [`TaskIssueType`] requires the configured Issue Type ID *and* name. A
    /// renamed or recreated account therefore stops matching instead of
    /// silently inheriting trust.
    pub fn matches(&self, author: Option<&Value>) -> bool {
        author.is_some_and(|author| {
            author.get("node_id").and_then(Value::as_str) == Some(self.id.as_str())
                && author.get("login").and_then(Value::as_str) == Some(self.login.as_str())
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseTrust {
    #[serde(rename = "assigners")]
    pub dispatchers: Vec<TrustedIdentity>,
    pub reporters: Vec<TrustedIdentity>,
}

impl LeaseTrust {
    pub fn validate(&self) -> Result<()> {
        for (name, identities) in [
            ("protocolTrust.assigners", &self.dispatchers),
            ("protocolTrust.reporters", &self.reporters),
        ] {
            ensure!(
                !identities.is_empty(),
                "{name} must list at least one trusted identity"
            );
            for (index, identity) in identities.iter().enumerate() {
                identity.validate(&format!("{name}[{index}]"))?;
            }
            let mut ids: Vec<&str> = identities.iter().map(|entry| entry.id.as_str()).collect();
            ids.sort_unstable();
            let unique = ids.len();
            ids.dedup();
            ensure!(ids.len() == unique, "{name} must not repeat an identity ID");
        }
        Ok(())
    }

    pub fn is_assigner(&self, author: Option<&Value>) -> bool {
        self.dispatchers
            .iter()
            .any(|identity| identity.matches(author))
    }

    pub fn is_reporter(&self, author: Option<&Value>) -> bool {
        self.reporters
            .iter()
            .any(|identity| identity.matches(author))
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentConfig {
    /// `owner/name` of the repository holding the agent file.
    pub repository: String,
    /// The exact git ref (normally a branch such as `main`).
    pub r#ref: String,
    /// The exact repository-relative path of the agent file.
    pub path: String,
    /// A read-only GitHub credential used only to read the agent file. It is
    /// the same bearer-token mechanism the bootstrapper already uses.
    pub token: String,
    /// GraphQL API endpoint. Override for GitHub Enterprise Server.
    #[serde(default = "default_agent_api_base_url")]
    pub api_base_url: String,
}

fn default_agent_api_base_url() -> String {
    DEFAULT_AGENT_API_BASE_URL.to_string()
}

/// Default GitHub GraphQL API endpoint used to read the agent file.
pub const DEFAULT_AGENT_API_BASE_URL: &str = "https://api.github.com/graphql";

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            repository: String::new(),
            r#ref: String::new(),
            path: String::new(),
            token: String::new(),
            api_base_url: default_agent_api_base_url(),
        }
    }
}

impl AgentConfig {
    pub fn location(&self) -> AgentFileLocation {
        AgentFileLocation {
            repository: self.repository.clone(),
            r#ref: self.r#ref.clone(),
            path: self.path.clone(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.location().validate()?;
        ensure!(
            !self.token.trim().is_empty(),
            "agentConfig.token cannot be empty"
        );
        ensure!(
            !self.api_base_url.trim().is_empty(),
            "agentConfig.apiBaseUrl cannot be empty"
        );
        Ok(())
    }
}

/// Default path for the VNext workflow definition file.
pub const DEFAULT_WORKFLOW_DEFINITION_PATH: &str =
    ".github/workgraph/workflows/issue-lifecycle-vnext.body";

/// Configuration for fetching the bounded VNext workflow definition from a
/// GitHub repository file.
///
/// The fields mirror [`AgentConfig`] and use the same validation and
/// transport ([`crate::agent_client::AgentFileClient`]).
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowDefinitionConfig {
    /// `owner/name` of the repository holding the definition file.
    pub repository: String,
    /// The exact git ref (normally a branch such as `main`).
    pub r#ref: String,
    /// Repository-relative path of the definition file.
    #[serde(default = "default_workflow_definition_path")]
    pub path: String,
    /// A read-only GitHub credential used to read the file.
    pub token: String,
    /// GraphQL API endpoint. Override for GitHub Enterprise Server.
    #[serde(default = "default_agent_api_base_url")]
    pub api_base_url: String,
}

fn default_workflow_definition_path() -> String {
    DEFAULT_WORKFLOW_DEFINITION_PATH.to_string()
}

impl Default for WorkflowDefinitionConfig {
    fn default() -> Self {
        Self {
            repository: String::new(),
            r#ref: String::new(),
            path: default_workflow_definition_path(),
            token: String::new(),
            api_base_url: default_agent_api_base_url(),
        }
    }
}

impl WorkflowDefinitionConfig {
    /// Build an [`AgentFileLocation`] for reuse with the generalized fetch
    /// client.
    pub fn location(&self) -> AgentFileLocation {
        AgentFileLocation {
            repository: self.repository.clone(),
            r#ref: self.r#ref.clone(),
            path: self.path.clone(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.location().validate()?;
        ensure!(
            !self.token.trim().is_empty(),
            "workflowDefinition.token cannot be empty"
        );
        ensure!(
            !self.api_base_url.trim().is_empty(),
            "workflowDefinition.apiBaseUrl cannot be empty"
        );
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubWorkGraphSourceConfig {
    pub organization: String,
    pub task_issue_type: TaskIssueType,
    #[serde(default)]
    pub repositories: Vec<String>,
    /// Location and credential of the agent-capacity configuration file.
    ///
    /// Optional: a deployment that does not run the agent queue omits it and
    /// projects no agent or slot nodes at all. When it *is* present it is
    /// strictly required — a malformed or unreadable file never degrades into
    /// a silently empty agent pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_config: Option<AgentConfig>,
    #[serde(
        default,
        rename = "protocolTrust",
        skip_serializing_if = "Option::is_none"
    )]
    pub lease_trust: Option<LeaseTrust>,
    pub webhook: WebhookConfig,
    #[serde(default, with = "DurabilityConfigDef")]
    pub durability: DurabilityConfig,
    /// VNext workflow definition file configuration.
    ///
    /// When present, the source fetches and projects the definition at
    /// startup and on matching push deliveries. Requires a
    /// `WorkGraphProjector` to be injected via the builder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_definition: Option<WorkflowDefinitionConfig>,
}

impl Default for GitHubWorkGraphSourceConfig {
    fn default() -> Self {
        Self {
            organization: String::new(),
            task_issue_type: TaskIssueType::default(),
            repositories: Vec::new(),
            agent_config: None,
            lease_trust: None,
            webhook: WebhookConfig::default(),
            durability: DurabilityConfig {
                enabled: true,
                ..DurabilityConfig::default()
            },
            workflow_definition: None,
        }
    }
}

impl GitHubWorkGraphSourceConfig {
    pub fn validate(&self) -> Result<()> {
        let org = &self.organization;
        ensure!(!org.trim().is_empty(), "organization cannot be empty");
        let single = !org.contains('/') && org.trim() == org;
        ensure!(single, "organization must be one GitHub organization login");
        let secret = &self.webhook.secret;
        ensure!(!secret.trim().is_empty(), "webhook.secret cannot be empty");
        ensure!(
            !self.webhook.lease_validation_token.trim().is_empty(),
            "webhook.leaseValidationToken cannot be empty"
        );
        ensure!(
            self.webhook.lease_validation_token != *secret,
            "webhook.leaseValidationToken must differ from webhook.secret"
        );
        let path = &self.webhook.path;
        ensure!(path.starts_with('/'), "webhook.path must start with '/'");
        let static_path = path
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte));
        ensure!(static_path, "webhook.path must be a static URL path");
        ensure!(
            self.webhook.body_limit_bytes > 0,
            "webhook.body_limit_bytes must be > 0"
        );
        ensure!(self.durability.enabled, "durability.enabled must be true");
        ensure!(
            self.durability.capacity_policy == CapacityPolicy::RejectIncoming,
            "durability.capacityPolicy must be RejectIncoming"
        );
        self.task_issue_type.validate()?;
        if let Some(agent_config) = &self.agent_config {
            agent_config.validate()?;
        }
        if let Some(lease_trust) = &self.lease_trust {
            lease_trust.validate()?;
            ensure!(
                self.agent_config.is_some(),
                "protocolTrust requires agentConfig"
            );
        }
        RepositoryFilter::new(org, &self.repositories)?;
        if let Some(workflow_definition) = &self.workflow_definition {
            workflow_definition.validate()?;
            let (owner, name) = workflow_definition
                .repository
                .split_once('/')
                .expect("validated repository contains one slash");
            ensure!(
                owner.eq_ignore_ascii_case(org),
                "workflowDefinition.repository must belong to the configured organization"
            );
            let filter = RepositoryFilter::new(org, &self.repositories)?;
            ensure!(
                filter.includes_name(name),
                "workflowDefinition.repository must be included by repositories"
            );
        }
        Ok(())
    }

    pub fn normalized(mut self) -> Result<Self> {
        self.validate()?;
        self.repositories =
            RepositoryFilter::new(&self.organization, &self.repositories)?.canonical_names();
        Ok(self)
    }

    pub fn repository_filter(&self) -> Result<RepositoryFilter> {
        RepositoryFilter::new(&self.organization, &self.repositories)
    }
}
