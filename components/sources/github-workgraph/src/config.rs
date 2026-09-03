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
use crate::protocol::{
    LEGACY_WORKFLOW_MAPPING_ID, WORKGRAPH_ADMISSION_LABEL, WORKGRAPH_ERROR_LABEL,
    WORKGRAPH_IGNORE_LABEL, WORKGRAPH_LABEL_PREFIX,
};
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
pub struct ProtocolTrust {
    pub task_creators: Vec<TrustedIdentity>,
    #[serde(rename = "assigners")]
    pub dispatchers: Vec<TrustedIdentity>,
    pub reporters: Vec<TrustedIdentity>,
}

impl ProtocolTrust {
    pub fn validate(&self) -> Result<()> {
        for (name, identities) in [
            ("protocolTrust.taskCreators", &self.task_creators),
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

    pub fn is_task_creator(&self, author: Option<&Value>) -> bool {
        self.task_creators
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
    /// a dedicated read-only bearer token.
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

/// A resolved read-only GitHub credential used *only* for authoritative Issue
/// reads during ambiguous or reordered webhook deliveries.
///
/// It never reads a workflow definition, an agent file, or any repository
/// content: the Reaction owns definition ownership and the agent sync path owns
/// the agent file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmissionReadCredential {
    pub token: String,
    pub api_base_url: String,
}

impl From<&AdmissionReadCredential> for AdmissionReadCredential {
    fn from(credential: &AdmissionReadCredential) -> Self {
        credential.clone()
    }
}

/// The smallest explicit read credential a Source needs for authoritative
/// Issue reads.
///
/// A mapping-only deployment configures no legacy `workflowDefinition` block
/// and therefore has no credential of its own. `admissionRead` supplies exactly
/// the token and API endpoint the authoritative read path needs, and nothing
/// else — mapping entries never carry credentials.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdmissionReadConfig {
    /// A read-only GitHub credential used to read authoritative Issue state.
    pub token: String,
    /// GraphQL API endpoint. Override for GitHub Enterprise Server.
    #[serde(default = "default_agent_api_base_url")]
    pub api_base_url: String,
}

impl Default for AdmissionReadConfig {
    fn default() -> Self {
        Self {
            token: String::new(),
            api_base_url: default_agent_api_base_url(),
        }
    }
}

impl AdmissionReadConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.token.trim().is_empty(),
            "admissionRead.token cannot be empty"
        );
        ensure!(
            !self.api_base_url.trim().is_empty(),
            "admissionRead.apiBaseUrl cannot be empty"
        );
        Ok(())
    }
}

impl From<&AdmissionReadConfig> for AdmissionReadCredential {
    fn from(config: &AdmissionReadConfig) -> Self {
        Self {
            token: config.token.clone(),
            api_base_url: config.api_base_url.clone(),
        }
    }
}

impl From<&AgentConfig> for AdmissionReadCredential {
    fn from(config: &AgentConfig) -> Self {
        Self {
            token: config.token.clone(),
            api_base_url: config.api_base_url.clone(),
        }
    }
}

/// Default path of the pinned WorkGraph workflow definition file.
///
/// Retained for configuration compatibility only. The Source never reads this
/// file; the Reaction loads the pinned definition it derives lifecycle from.
pub const DEFAULT_WORKFLOW_DEFINITION_PATH: &str =
    ".github/workgraph/workflows/issue-lifecycle-v1.body";

/// The pinned WorkGraph workflow definition location and the read-only GitHub
/// credential the Source uses for authoritative Issue-label reads.
///
/// The Source neither fetches nor projects the definition file: definition
/// ownership belongs to the Reaction, which loads and indexes the pinned
/// `WorkGraphWorkflowDefinition/v1` body itself. `repository`, `ref`, and
/// `path` are still validated and still pin the same immutable definition
/// identity the Reaction is configured with, so a deployment cannot silently
/// point the two at different workflows; they are otherwise ignored.
///
/// The fields mirror [`AgentConfig`] and use the same validation.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowDefinitionConfig {
    /// `owner/name` of the repository holding the pinned definition file.
    pub repository: String,
    /// The exact git ref (normally an immutable commit OID).
    pub r#ref: String,
    /// Repository-relative path of the pinned definition file. Never fetched.
    #[serde(default = "default_workflow_definition_path")]
    pub path: String,
    /// A read-only GitHub credential used to read authoritative Issue-label
    /// state during ambiguous webhook ordering transitions.
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
    /// Build an [`AgentFileLocation`] so the pinned definition location reuses
    /// the same repository/ref/path validation as the agent file. The Source
    /// never fetches this location.
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

impl From<&WorkflowDefinitionConfig> for AdmissionReadCredential {
    fn from(config: &WorkflowDefinitionConfig) -> Self {
        Self {
            token: config.token.clone(),
            api_base_url: config.api_base_url.clone(),
        }
    }
}

/// A pinned WorkGraph workflow definition *location* with no credential.
///
/// The Source never fetches, parses, or interprets the addressed file. The
/// location is validated and projected verbatim so the Reaction — which owns
/// every definition-dependent decision — knows which pinned definition a
/// mapping activation belongs to.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowDefinitionLocation {
    /// `owner/name` of the repository holding the pinned definition file.
    pub repository: String,
    /// The exact git ref (normally an immutable commit OID).
    pub r#ref: String,
    /// Repository-relative path of the pinned definition file. Never fetched.
    #[serde(default = "default_workflow_definition_path")]
    pub path: String,
}

impl WorkflowDefinitionLocation {
    pub fn location(&self) -> AgentFileLocation {
        AgentFileLocation {
            repository: self.repository.clone(),
            r#ref: self.r#ref.clone(),
            path: self.path.clone(),
        }
    }
}

/// Maximum length of a mapping ID and of the bounded name half of a selector
/// label. Both are configuration identifiers, never free-form text.
pub const MAX_WORKFLOW_MAPPING_NAME_LEN: usize = 64;

/// One configured Source label→workflow mapping.
///
/// Recognizing `label` on an ordinary Issue admits that Issue as a Root
/// candidate and creates an independent admission generation for this mapping.
/// The Source validates and remembers the mapping; it never reads the
/// definition the mapping points at.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowMappingConfig {
    /// Stable mapping identity. Must be unique and must not be the reserved
    /// legacy ID `workgraph`.
    pub id: String,
    /// Exact, case-sensitive selector label of the form
    /// `workgraph:<bounded-name>`. Must be unique and must not be one of the
    /// reserved `workgraph:ignore` / `workgraph:error` modifiers.
    pub label: String,
    /// The pinned definition location this mapping projects for the Reaction.
    pub workflow_definition: WorkflowDefinitionLocation,
}

fn valid_bounded_mapping_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_WORKFLOW_MAPPING_NAME_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte))
}

impl WorkflowMappingConfig {
    pub fn validate(&self, index: usize) -> Result<()> {
        let field = format!("workflowMappings[{index}]");
        ensure!(
            valid_bounded_mapping_name(&self.id),
            "{field}.id must be 1..={MAX_WORKFLOW_MAPPING_NAME_LEN} ASCII letters, digits, '.', \
             '-', or '_'"
        );
        ensure!(
            self.id != LEGACY_WORKFLOW_MAPPING_ID,
            "{field}.id must not be the reserved legacy mapping ID '{LEGACY_WORKFLOW_MAPPING_ID}'"
        );
        ensure!(
            !matches!(
                self.label.as_str(),
                WORKGRAPH_IGNORE_LABEL | WORKGRAPH_ERROR_LABEL
            ),
            "{field}.label must not be the reserved exclusion modifier '{}'",
            self.label
        );
        ensure!(
            self.label != WORKGRAPH_ADMISSION_LABEL,
            "{field}.label must not be the legacy admission label \
             '{WORKGRAPH_ADMISSION_LABEL}'; configure workflowDefinition instead"
        );
        let name = self
            .label
            .strip_prefix(WORKGRAPH_LABEL_PREFIX)
            .ok_or_else(|| anyhow::anyhow!("{field}.label must be exactly 'workgraph:<name>'"))?;
        ensure!(
            valid_bounded_mapping_name(name),
            "{field}.label name must be 1..={MAX_WORKFLOW_MAPPING_NAME_LEN} ASCII letters, \
             digits, '.', '-', or '_'"
        );
        self.workflow_definition
            .location()
            .validate()
            .map_err(|error| anyhow::anyhow!("{field}.workflowDefinition: {error}"))?;
        Ok(())
    }
}

/// One resolved mapping the ingress path matches observed labels against.
///
/// The legacy top-level `workflowDefinition` block resolves to an implicit
/// mapping with ID [`LEGACY_WORKFLOW_MAPPING_ID`] selected by the exact
/// `workgraph` label.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ResolvedWorkflowMapping {
    pub id: String,
    pub label: String,
    pub definition_repository: String,
    pub definition_ref: String,
    pub definition_path: String,
}

/// The complete ordered set of mappings a Source recognizes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkflowMappingSet {
    mappings: Vec<ResolvedWorkflowMapping>,
}

impl WorkflowMappingSet {
    pub fn new(mappings: Vec<ResolvedWorkflowMapping>) -> Self {
        let mut mappings = mappings;
        mappings.sort();
        Self { mappings }
    }

    pub fn is_empty(&self) -> bool {
        self.mappings.is_empty()
    }

    pub fn len(&self) -> usize {
        self.mappings.len()
    }

    /// Every mapping, ordered by mapping ID.
    pub fn all(&self) -> &[ResolvedWorkflowMapping] {
        &self.mappings
    }

    /// The mapping selected by an exact, case-sensitive label.
    pub fn by_label(&self, label: &str) -> Option<&ResolvedWorkflowMapping> {
        self.mappings.iter().find(|mapping| mapping.label == label)
    }

    /// Whether an exact label activates any configured mapping. Reserved
    /// exclusion modifiers and unknown `workgraph:*` labels never do.
    pub fn recognizes_label(&self, label: &str) -> bool {
        self.by_label(label).is_some()
    }

    /// Every mapping activated by an exact label set, ordered by mapping ID.
    pub fn active_for_labels<'a, I>(&self, labels: I) -> Vec<&ResolvedWorkflowMapping>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let observed: BTreeSet<&str> = labels.into_iter().collect();
        self.mappings
            .iter()
            .filter(|mapping| observed.contains(mapping.label.as_str()))
            .collect()
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
    pub protocol_trust: Option<ProtocolTrust>,
    pub webhook: WebhookConfig,
    #[serde(default, with = "DurabilityConfigDef")]
    pub durability: DurabilityConfig,
    /// Pinned WorkGraph workflow definition location plus the read-only
    /// GitHub credential used for authoritative Issue-label reads.
    ///
    /// The definition file itself is never fetched or projected: the Reaction
    /// owns the pinned definition. When absent, ambiguous label-ordering
    /// transitions cannot be resolved against GitHub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_definition: Option<WorkflowDefinitionConfig>,
    /// Explicit read-only GitHub credential for authoritative Issue reads.
    ///
    /// Required by a mapping-only deployment, which configures no legacy
    /// `workflowDefinition` block and therefore inherits no credential — unless
    /// a repository-compatible [`Self::agent_config`] already supplies one.
    /// It is never used to read a workflow definition or any other file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_read: Option<AdmissionReadConfig>,
    /// Configured label→workflow mappings.
    ///
    /// Each entry recognizes one exact `workgraph:<name>` selector label and
    /// admits an ordinary Issue carrying it as a Root candidate with its own
    /// independent admission generation. The Source validates and projects the
    /// mapping's definition location; it never fetches or interprets it.
    ///
    /// The legacy top-level [`Self::workflow_definition`] block remains a
    /// backwards-compatible implicit mapping for the exact `workgraph` label.
    /// A deployment must configure at least one of the two.
    #[serde(default)]
    pub workflow_mappings: Vec<WorkflowMappingConfig>,
}

impl Default for GitHubWorkGraphSourceConfig {
    fn default() -> Self {
        Self {
            organization: String::new(),
            task_issue_type: TaskIssueType::default(),
            repositories: Vec::new(),
            agent_config: None,
            protocol_trust: None,
            webhook: WebhookConfig::default(),
            durability: DurabilityConfig {
                enabled: true,
                ..DurabilityConfig::default()
            },
            workflow_definition: None,
            admission_read: None,
            workflow_mappings: Vec::new(),
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
        if let Some(protocol_trust) = &self.protocol_trust {
            protocol_trust.validate()?;
            ensure!(
                self.agent_config.is_some(),
                "protocolTrust requires agentConfig"
            );
        }
        RepositoryFilter::new(org, &self.repositories)?;
        let filter = RepositoryFilter::new(org, &self.repositories)?;
        let mut check_definition_repository = |field: &str, repository: &str| -> Result<()> {
            let (owner, name) = repository
                .split_once('/')
                .expect("validated repository contains one slash");
            ensure!(
                owner.eq_ignore_ascii_case(org),
                "{field}.repository must belong to the configured organization"
            );
            ensure!(
                filter.includes_name(name),
                "{field}.repository must be included by repositories"
            );
            Ok(())
        };
        if let Some(workflow_definition) = &self.workflow_definition {
            workflow_definition.validate()?;
            check_definition_repository("workflowDefinition", &workflow_definition.repository)?;
        }
        let mut seen_ids = BTreeSet::new();
        let mut seen_labels = BTreeSet::new();
        for (index, mapping) in self.workflow_mappings.iter().enumerate() {
            mapping.validate(index)?;
            ensure!(
                seen_ids.insert(mapping.id.as_str()),
                "workflowMappings[{index}].id '{}' is not unique",
                mapping.id
            );
            ensure!(
                seen_labels.insert(mapping.label.as_str()),
                "workflowMappings[{index}].label '{}' is not unique",
                mapping.label
            );
            check_definition_repository(
                &format!("workflowMappings[{index}].workflowDefinition"),
                &mapping.workflow_definition.repository,
            )?;
        }
        if let Some(admission_read) = &self.admission_read {
            admission_read.validate()?;
        }
        // Root admission is only correct when ambiguous or reordered label
        // deliveries can be resolved against GitHub. A legacy deployment always
        // had `workflowDefinition.token`; a mapping-only deployment must supply
        // the same capability explicitly instead of silently degrading.
        ensure!(
            self.workflow_mapping_set().is_empty() || self.admission_read_credential().is_some(),
            "an admitting Source requires a read-only GitHub credential for authoritative Issue \
             reads: configure admissionRead.token, the legacy workflowDefinition, or an \
             agentConfig whose repository is inside the configured organization and repositories \
             allowlist"
        );
        Ok(())
    }

    /// The read-only credential the authoritative Issue-read path uses.
    ///
    /// Resolution is deterministic and least-surprising:
    /// 1. an explicit `admissionRead` block always wins;
    /// 2. otherwise the legacy `workflowDefinition` credential, preserving
    ///    existing deployments byte for byte;
    /// 3. otherwise a *repository-compatible* `agentConfig` credential.
    ///
    /// Reuse of the agent credential is deliberately narrow: it is only safe
    /// when the agent file already lives in the configured organization and
    /// inside the `repositories` allowlist, which is exactly the scope the
    /// authoritative Issue reads stay within. An agent file hosted anywhere
    /// else is never reused, so a token scoped to an unrelated repository is
    /// never sent to an Issue-read endpoint on the operator's behalf.
    pub fn admission_read_credential(&self) -> Option<AdmissionReadCredential> {
        if let Some(explicit) = &self.admission_read {
            return Some(explicit.into());
        }
        if let Some(definition) = &self.workflow_definition {
            return Some(definition.into());
        }
        self.repository_compatible_agent_credential()
    }

    fn repository_compatible_agent_credential(&self) -> Option<AdmissionReadCredential> {
        let agent_config = self.agent_config.as_ref()?;
        let (owner, name) = agent_config.repository.split_once('/')?;
        if !owner.eq_ignore_ascii_case(&self.organization) {
            return None;
        }
        self.repository_filter()
            .ok()?
            .includes_name(name)
            .then(|| agent_config.into())
    }

    /// The complete ordered mapping set this configuration recognizes.
    ///
    /// The legacy `workflowDefinition` block contributes one implicit mapping
    /// with ID [`LEGACY_WORKFLOW_MAPPING_ID`] selected by the exact
    /// `workgraph` label. Configured `workflowMappings` contribute the rest.
    /// The two label spaces are disjoint by construction, so configuring both
    /// is unambiguous.
    pub fn workflow_mapping_set(&self) -> WorkflowMappingSet {
        let legacy = self
            .workflow_definition
            .as_ref()
            .map(|definition| ResolvedWorkflowMapping {
                id: LEGACY_WORKFLOW_MAPPING_ID.to_string(),
                label: WORKGRAPH_ADMISSION_LABEL.to_string(),
                definition_repository: definition.repository.clone(),
                definition_ref: definition.r#ref.clone(),
                definition_path: definition.path.clone(),
            });
        let configured = self
            .workflow_mappings
            .iter()
            .map(|mapping| ResolvedWorkflowMapping {
                id: mapping.id.clone(),
                label: mapping.label.clone(),
                definition_repository: mapping.workflow_definition.repository.clone(),
                definition_ref: mapping.workflow_definition.r#ref.clone(),
                definition_path: mapping.workflow_definition.path.clone(),
            });
        WorkflowMappingSet::new(legacy.into_iter().chain(configured).collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(path: &str) -> WorkflowDefinitionLocation {
        WorkflowDefinitionLocation {
            repository: "acme/widgets".to_string(),
            r#ref: "main".to_string(),
            path: path.to_string(),
        }
    }

    fn mapping(id: &str, label: &str, path: &str) -> WorkflowMappingConfig {
        WorkflowMappingConfig {
            id: id.to_string(),
            label: label.to_string(),
            workflow_definition: definition(path),
        }
    }

    fn legacy_definition() -> WorkflowDefinitionConfig {
        WorkflowDefinitionConfig {
            repository: "acme/widgets".to_string(),
            r#ref: "main".to_string(),
            path: DEFAULT_WORKFLOW_DEFINITION_PATH.to_string(),
            token: "read-token".to_string(),
            api_base_url: DEFAULT_AGENT_API_BASE_URL.to_string(),
        }
    }

    fn source_config(mappings: Vec<WorkflowMappingConfig>) -> GitHubWorkGraphSourceConfig {
        GitHubWorkGraphSourceConfig {
            organization: "acme".to_string(),
            task_issue_type: TaskIssueType {
                id: "IT_task".to_string(),
                name: "WorkGraphTask".to_string(),
            },
            repositories: vec!["widgets".to_string()],
            webhook: WebhookConfig {
                secret: "webhook-secret".to_string(),
                lease_validation_token: "lease-secret".to_string(),
                ..WebhookConfig::default()
            },
            admission_read: (!mappings.is_empty()).then(|| AdmissionReadConfig {
                token: "issue-read-token".to_string(),
                ..AdmissionReadConfig::default()
            }),
            workflow_mappings: mappings,
            ..GitHubWorkGraphSourceConfig::default()
        }
    }

    fn agent_config(repository: &str) -> AgentConfig {
        AgentConfig {
            repository: repository.to_string(),
            r#ref: "main".to_string(),
            path: ".github/workgraph/agents.yaml".to_string(),
            token: "agent-read-token".to_string(),
            api_base_url: DEFAULT_AGENT_API_BASE_URL.to_string(),
        }
    }

    #[test]
    fn mappings_only_configuration_is_accepted() {
        let config = source_config(vec![
            mapping(
                "foo",
                "workgraph:foo",
                ".github/workgraph/workflows/foo-v1.body",
            ),
            mapping(
                "bar",
                "workgraph:bar",
                ".github/workgraph/workflows/bar-v1.body",
            ),
        ]);
        config.validate().expect("mappings-only config is valid");
        let set = config.workflow_mapping_set();
        assert_eq!(set.len(), 2);
        assert_eq!(
            set.all().iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["bar", "foo"],
            "mappings are ordered by mapping ID"
        );
        assert!(set.recognizes_label("workgraph:foo"));
        assert!(!set.recognizes_label("workgraph"));
    }

    // ── Authoritative Issue-read credential ───────────────────────────────

    #[test]
    fn a_mappings_only_configuration_resolves_its_explicit_read_credential() {
        let config = source_config(vec![mapping("foo", "workgraph:foo", "foo.body")]);
        assert!(config.workflow_definition.is_none());
        let credential = config
            .admission_read_credential()
            .expect("mappings-only config resolves a read credential");
        assert_eq!(credential.token, "issue-read-token");
        assert_eq!(credential.api_base_url, DEFAULT_AGENT_API_BASE_URL);
    }

    #[test]
    fn a_mappings_only_configuration_without_any_read_credential_is_rejected() {
        let mut config = source_config(vec![mapping("foo", "workgraph:foo", "foo.body")]);
        config.admission_read = None;
        assert!(config
            .validate()
            .expect_err("no authoritative read credential")
            .to_string()
            .contains("authoritative Issue"));
        assert!(config.admission_read_credential().is_none());
    }

    #[test]
    fn an_explicit_read_credential_wins_over_the_legacy_definition_token() {
        let mut config = source_config(vec![mapping("foo", "workgraph:foo", "foo.body")]);
        config.workflow_definition = Some(legacy_definition());
        config.admission_read = Some(AdmissionReadConfig {
            token: "explicit-token".to_string(),
            api_base_url: "https://ghes.example.com/api/graphql".to_string(),
        });
        config.validate().expect("both may be configured");
        let credential = config.admission_read_credential().expect("credential");
        assert_eq!(credential.token, "explicit-token");
        assert_eq!(
            credential.api_base_url,
            "https://ghes.example.com/api/graphql"
        );
    }

    #[test]
    fn a_repository_compatible_agent_credential_is_reused_for_issue_reads() {
        let mut config = source_config(vec![mapping("foo", "workgraph:foo", "foo.body")]);
        config.admission_read = None;
        config.agent_config = Some(agent_config("acme/widgets"));
        config
            .validate()
            .expect("a repository-compatible agent credential satisfies the requirement");
        let credential = config.admission_read_credential().expect("credential");
        assert_eq!(credential.token, "agent-read-token");
        assert_eq!(credential.api_base_url, DEFAULT_AGENT_API_BASE_URL);
    }

    #[test]
    fn an_agent_credential_outside_the_allowlist_is_never_reused() {
        for repository in ["acme/other", "evil/widgets"] {
            let mut config = source_config(vec![mapping("foo", "workgraph:foo", "foo.body")]);
            config.admission_read = None;
            config.agent_config = Some(agent_config(repository));
            assert!(
                config.admission_read_credential().is_none(),
                "'{repository}' is not repository-compatible"
            );
            assert!(
                config.validate().is_err(),
                "'{repository}' must not satisfy the read-credential requirement"
            );
        }
    }

    #[test]
    fn a_legacy_only_configuration_keeps_using_its_definition_token() {
        let mut config = source_config(Vec::new());
        config.workflow_definition = Some(legacy_definition());
        config.agent_config = Some(agent_config("acme/widgets"));
        config.validate().expect("legacy-only config is valid");
        assert_eq!(
            config
                .admission_read_credential()
                .expect("credential")
                .token,
            "read-token",
            "an existing deployment keeps the exact credential it already used"
        );
    }

    #[test]
    fn a_non_admitting_configuration_needs_no_read_credential() {
        let config = source_config(Vec::new());
        assert!(config.workflow_mapping_set().is_empty());
        config
            .validate()
            .expect("a Source that can never admit a Root needs no Issue-read credential");
    }

    #[test]
    fn an_explicit_read_credential_must_be_complete() {
        let mut config = source_config(vec![mapping("foo", "workgraph:foo", "foo.body")]);
        config.admission_read = Some(AdmissionReadConfig {
            token: "  ".to_string(),
            ..AdmissionReadConfig::default()
        });
        assert!(config
            .validate()
            .expect_err("empty token")
            .to_string()
            .contains("admissionRead.token cannot be empty"));
        config.admission_read = Some(AdmissionReadConfig {
            token: "issue-read-token".to_string(),
            api_base_url: String::new(),
        });
        assert!(config
            .validate()
            .expect_err("empty endpoint")
            .to_string()
            .contains("admissionRead.apiBaseUrl cannot be empty"));
    }

    #[test]
    fn a_mapping_entry_can_never_carry_a_credential() {
        let error = serde_json::from_value::<WorkflowMappingConfig>(serde_json::json!({
            "id": "foo",
            "label": "workgraph:foo",
            "workflowDefinition": {
                "repository": "acme/widgets",
                "ref": "main",
                "path": "foo.body",
                "token": "leaked"
            }
        }))
        .expect_err("a mapping definition location has no token field");
        assert!(error.to_string().contains("unknown field `token`"));
    }

    #[test]
    fn legacy_only_configuration_remains_an_implicit_workgraph_mapping() {
        let mut config = source_config(Vec::new());
        config.workflow_definition = Some(legacy_definition());
        config.validate().expect("legacy-only config is valid");
        let set = config.workflow_mapping_set();
        assert_eq!(set.len(), 1);
        let legacy = set.by_label(WORKGRAPH_ADMISSION_LABEL).expect("legacy");
        assert_eq!(legacy.id, LEGACY_WORKFLOW_MAPPING_ID);
        assert_eq!(legacy.definition_path, DEFAULT_WORKFLOW_DEFINITION_PATH);
        assert!(!set.recognizes_label("workgraph:foo"));
    }

    #[test]
    fn legacy_and_mappings_together_stay_unambiguous() {
        let mut config = source_config(vec![mapping(
            "foo",
            "workgraph:foo",
            ".github/workgraph/workflows/foo-v1.body",
        )]);
        config.workflow_definition = Some(legacy_definition());
        config.validate().expect("both may be configured");
        let set = config.workflow_mapping_set();
        assert_eq!(set.len(), 2);
        assert!(set.recognizes_label(WORKGRAPH_ADMISSION_LABEL));
        assert!(set.recognizes_label("workgraph:foo"));
    }

    #[test]
    fn mapping_ids_must_be_unique() {
        let config = source_config(vec![
            mapping("foo", "workgraph:foo", "a.body"),
            mapping("foo", "workgraph:other", "b.body"),
        ]);
        assert!(config
            .validate()
            .expect_err("duplicate mapping ID")
            .to_string()
            .contains("is not unique"));
    }

    #[test]
    fn mapping_labels_must_be_unique() {
        let config = source_config(vec![
            mapping("foo", "workgraph:foo", "a.body"),
            mapping("other", "workgraph:foo", "b.body"),
        ]);
        assert!(config
            .validate()
            .expect_err("duplicate selector label")
            .to_string()
            .contains("is not unique"));
    }

    #[test]
    fn reserved_exclusion_labels_are_rejected_as_selectors() {
        for label in [WORKGRAPH_IGNORE_LABEL, WORKGRAPH_ERROR_LABEL] {
            let config = source_config(vec![mapping("foo", label, "a.body")]);
            assert!(
                config
                    .validate()
                    .expect_err("reserved exclusion modifier")
                    .to_string()
                    .contains("reserved exclusion modifier"),
                "{label} must be rejected"
            );
        }
    }

    #[test]
    fn legacy_label_and_legacy_mapping_id_are_reserved() {
        let config = source_config(vec![mapping("foo", WORKGRAPH_ADMISSION_LABEL, "a.body")]);
        assert!(config
            .validate()
            .expect_err("legacy label")
            .to_string()
            .contains("must not be the legacy admission label"));
        let config = source_config(vec![mapping(
            LEGACY_WORKFLOW_MAPPING_ID,
            "workgraph:foo",
            "a.body",
        )]);
        assert!(config
            .validate()
            .expect_err("legacy mapping ID")
            .to_string()
            .contains("reserved legacy mapping ID"));
    }

    #[test]
    fn selector_labels_must_be_exact_bounded_and_case_sensitive() {
        for label in [
            "workgraph",
            "WorkGraph:foo",
            "workgraph:",
            "workgraph:foo bar",
            "workgraph:foo:bar",
            "prefix-workgraph:foo",
            "workgraph:FOO/bar",
        ] {
            let config = source_config(vec![mapping("foo", label, "a.body")]);
            assert!(
                config.validate().is_err(),
                "'{label}' must not be a valid selector"
            );
        }
        // Case is preserved exactly: an upper-case name is a *different* label.
        let config = source_config(vec![
            mapping("foo", "workgraph:foo", "a.body"),
            mapping("foo-upper", "workgraph:FOO", "b.body"),
        ]);
        config
            .validate()
            .expect("case-sensitive labels are distinct");
    }

    #[test]
    fn selector_label_name_is_length_bounded() {
        let long = "a".repeat(MAX_WORKFLOW_MAPPING_NAME_LEN + 1);
        let config = source_config(vec![mapping("foo", &format!("workgraph:{long}"), "a.body")]);
        assert!(config.validate().is_err());
        let ok = "a".repeat(MAX_WORKFLOW_MAPPING_NAME_LEN);
        let config = source_config(vec![mapping("foo", &format!("workgraph:{ok}"), "a.body")]);
        config.validate().expect("bounded name is accepted");
    }

    #[test]
    fn mapping_definition_repository_must_stay_in_the_allowlist() {
        let mut entry = mapping("foo", "workgraph:foo", "a.body");
        entry.workflow_definition.repository = "acme/other".to_string();
        let config = source_config(vec![entry]);
        assert!(config
            .validate()
            .expect_err("outside allowlist")
            .to_string()
            .contains("must be included by repositories"));

        let mut entry = mapping("foo", "workgraph:foo", "a.body");
        entry.workflow_definition.repository = "evil/widgets".to_string();
        let config = source_config(vec![entry]);
        assert!(config
            .validate()
            .expect_err("outside organization")
            .to_string()
            .contains("must belong to the configured organization"));
    }

    #[test]
    fn distinct_mappings_may_share_one_definition_location() {
        let config = source_config(vec![
            mapping("foo", "workgraph:foo", "shared.body"),
            mapping("bar", "workgraph:bar", "shared.body"),
        ]);
        config.validate().expect("shared definitions are allowed");
        let set = config.workflow_mapping_set();
        let active = set.active_for_labels(["workgraph:foo", "workgraph:bar"]);
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].definition_path, active[1].definition_path);
        assert_ne!(active[0].id, active[1].id);
    }

    #[test]
    fn unknown_and_reserved_labels_never_activate_a_mapping() {
        let config = source_config(vec![mapping("foo", "workgraph:foo", "a.body")]);
        let set = config.workflow_mapping_set();
        let active = set.active_for_labels([
            "workgraph:unknown",
            WORKGRAPH_IGNORE_LABEL,
            WORKGRAPH_ERROR_LABEL,
            "workgraph",
        ]);
        assert!(active.is_empty());
        assert_eq!(set.active_for_labels(["workgraph:foo"]).len(), 1);
    }

    #[test]
    fn mapping_definition_path_must_be_a_normalized_relative_path() {
        for path in [
            "/abs.body",
            "../escape.body",
            "a//b.body",
            "with space.body",
        ] {
            let config = source_config(vec![mapping("foo", "workgraph:foo", path)]);
            assert!(config.validate().is_err(), "'{path}' must be rejected");
        }
    }
}
