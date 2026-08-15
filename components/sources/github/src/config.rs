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

//! Configuration for the GitHub source plugin.

use anyhow::{anyhow, Result};
use drasi_lib::wal::CapacityPolicy;
use drasi_lib::DurabilityConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_path() -> String {
    "/webhook".to_string()
}

fn default_body_limit() -> usize {
    10 * 1024 * 1024
}

fn default_graphql_url() -> String {
    "https://api.github.com/graphql".to_string()
}

/// GitHub project selector.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectSpec {
    /// Project owner (org or user login).
    pub owner: String,
    /// Project number.
    pub number: u32,
}

/// Webhook listener configuration.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookConfig {
    /// Listener host.
    #[serde(default = "default_host")]
    pub host: String,
    /// Listener port.
    #[serde(default = "default_port")]
    pub port: u16,
    /// Listener path.
    #[serde(default = "default_path")]
    pub path: String,
    /// Resolved webhook secret.
    pub secret: String,
    /// Maximum request body size in bytes.
    #[serde(default = "default_body_limit")]
    pub body_limit_bytes: usize,
}

impl fmt::Debug for WebhookConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebhookConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("path", &self.path)
            .field("secret", &"[REDACTED]")
            .field("body_limit_bytes", &self.body_limit_bytes)
            .finish()
    }
}

/// Source configuration for the authorized GitHub source.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubSourceConfig {
    /// Resolved GitHub PAT token.
    pub token: String,
    /// Explicit repositories to track (`owner/repo`).
    #[serde(default)]
    pub repositories: Vec<String>,
    /// Projects to track (`owner` + `number`).
    #[serde(default)]
    pub projects: Vec<ProjectSpec>,
    /// Webhook listener options.
    pub webhook: WebhookConfig,
    /// WAL durability settings (must be enabled for this source).
    #[serde(default)]
    pub durability: DurabilityConfig,
    /// GraphQL endpoint.
    #[serde(default = "default_graphql_url")]
    pub graphql_url: String,
}

impl fmt::Debug for GitHubSourceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHubSourceConfig")
            .field("token", &"[REDACTED]")
            .field("repositories", &self.repositories)
            .field("projects", &self.projects)
            .field("webhook", &self.webhook)
            .field("durability", &self.durability)
            .field("graphql_url", &self.graphql_url)
            .finish()
    }
}

impl Default for GitHubSourceConfig {
    fn default() -> Self {
        Self {
            token: String::new(),
            repositories: Vec::new(),
            projects: Vec::new(),
            webhook: WebhookConfig {
                host: default_host(),
                port: default_port(),
                path: default_path(),
                secret: String::new(),
                body_limit_bytes: default_body_limit(),
            },
            durability: DurabilityConfig {
                enabled: true,
                ..DurabilityConfig::default()
            },
            graphql_url: default_graphql_url(),
        }
    }
}

impl GitHubSourceConfig {
    /// Validate configuration and normalize repository references.
    pub fn validate(&self) -> Result<()> {
        if self.token.trim().is_empty() {
            return Err(anyhow!("token cannot be empty"));
        }
        if self.webhook.secret.trim().is_empty() {
            return Err(anyhow!("webhook.secret cannot be empty"));
        }

        if self.repositories.is_empty() && self.projects.is_empty() {
            return Err(anyhow!(
                "At least one project or repository must be configured"
            ));
        }

        if !self.webhook.path.starts_with('/') {
            return Err(anyhow!("webhook.path must start with '/'"));
        }

        if self.webhook.body_limit_bytes == 0 {
            return Err(anyhow!("webhook.body_limit_bytes must be > 0"));
        }

        if self.projects.iter().any(|p| p.owner.trim().is_empty()) {
            return Err(anyhow!("project.owner cannot be empty"));
        }

        for repo in &self.repositories {
            parse_repository_full_name(repo)?;
        }

        if !self.durability.enabled {
            return Err(anyhow!(
                "durability.enabled must be true for github source (durable WAL is mandatory)"
            ));
        }
        if self.durability.capacity_policy != CapacityPolicy::RejectIncoming {
            return Err(anyhow!(
                "durability.capacity_policy must be RejectIncoming for github source (OverwriteOldest is unsafe)"
            ));
        }

        Ok(())
    }

    /// Return normalized static repository set.
    pub fn static_repository_set(&self) -> Result<HashSet<String>> {
        let mut repos = HashSet::new();
        for repo in &self.repositories {
            let (owner, name) = parse_repository_full_name(repo)?;
            repos.insert(format!("{owner}/{name}"));
        }
        Ok(repos)
    }
}

/// Parse `owner/repo` into lowercase canonical form.
pub fn parse_repository_full_name(input: &str) -> Result<(String, String)> {
    let mut parts = input.split('/');
    let owner = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Repository must be in owner/repo format"))?;
    let repo = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Repository must be in owner/repo format"))?;
    if parts.next().is_some() {
        return Err(anyhow!("Repository must be in owner/repo format"));
    }
    Ok((owner.to_ascii_lowercase(), repo.to_ascii_lowercase()))
}
