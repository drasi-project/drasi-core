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
    pub body_limit_bytes: usize,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 8080,
            path: "/webhook".to_string(),
            secret: String::new(),
            body_limit_bytes: DEFAULT_BODY_LIMIT_BYTES,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubWorkGraphSourceConfig {
    pub organization: String,
    pub task_issue_type: TaskIssueType,
    #[serde(default)]
    pub repositories: Vec<String>,
    pub webhook: WebhookConfig,
    #[serde(default, with = "DurabilityConfigDef")]
    pub durability: DurabilityConfig,
}

impl Default for GitHubWorkGraphSourceConfig {
    fn default() -> Self {
        Self {
            organization: String::new(),
            task_issue_type: TaskIssueType::default(),
            repositories: Vec::new(),
            webhook: WebhookConfig::default(),
            durability: DurabilityConfig {
                enabled: true,
                ..DurabilityConfig::default()
            },
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
        RepositoryFilter::new(org, &self.repositories)?;
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
