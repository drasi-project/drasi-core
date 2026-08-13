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

//! Configuration types for the GitHub ProjectV2 item refresh reaction.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_graphql_url() -> String {
    "https://api.github.com/graphql".to_string()
}

fn default_request_timeout_ms() -> u64 {
    10_000
}

fn default_status_field_name() -> String {
    "Status".to_string()
}

fn default_delivery_record_ttl_secs() -> u64 {
    7 * 24 * 60 * 60
}

/// Runtime configuration for the reaction.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct GitHubProjectItemRefreshConfig {
    /// GitHub token used for GraphQL requests.
    pub github_token: String,
    /// GraphQL endpoint URL.
    #[serde(default = "default_graphql_url")]
    pub graphql_url: String,
    /// Additional GraphQL request headers.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub graphql_headers: HashMap<String, String>,
    /// Optional allowlist of project node IDs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowlisted_project_ids: Vec<String>,
    /// Name of the single-select project field representing status.
    #[serde(default = "default_status_field_name")]
    pub status_field_name: String,
    /// Standard-mode HTTP source event endpoint.
    pub destination_event_url: String,
    /// Optional bearer token used for destination HTTP source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_bearer_secret: Option<String>,
    /// Request timeout in milliseconds for GraphQL and destination HTTP calls.
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Retention window for terminal per-delivery records before durable pruning.
    #[serde(default = "default_delivery_record_ttl_secs")]
    pub delivery_record_ttl_secs: u64,
}

impl Default for GitHubProjectItemRefreshConfig {
    fn default() -> Self {
        Self {
            github_token: String::new(),
            graphql_url: default_graphql_url(),
            graphql_headers: HashMap::new(),
            allowlisted_project_ids: Vec::new(),
            status_field_name: default_status_field_name(),
            destination_event_url: String::new(),
            destination_bearer_secret: None,
            request_timeout_ms: default_request_timeout_ms(),
            delivery_record_ttl_secs: default_delivery_record_ttl_secs(),
        }
    }
}

impl std::fmt::Debug for GitHubProjectItemRefreshConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted_headers = self
            .graphql_headers
            .keys()
            .map(|name| (name.as_str(), "[REDACTED]"))
            .collect::<HashMap<_, _>>();

        f.debug_struct("GitHubProjectItemRefreshConfig")
            .field("github_token", &redacted_opt(!self.github_token.is_empty()))
            .field("graphql_url", &self.graphql_url)
            .field("graphql_headers", &redacted_headers)
            .field("allowlisted_project_ids", &self.allowlisted_project_ids)
            .field("status_field_name", &self.status_field_name)
            .field("destination_event_url", &self.destination_event_url)
            .field(
                "destination_bearer_secret",
                &redacted_opt(self.destination_bearer_secret.is_some()),
            )
            .field("request_timeout_ms", &self.request_timeout_ms)
            .field("delivery_record_ttl_secs", &self.delivery_record_ttl_secs)
            .finish()
    }
}

fn redacted_opt(is_set: bool) -> Option<&'static str> {
    if is_set {
        Some("[REDACTED]")
    } else {
        None
    }
}

impl GitHubProjectItemRefreshConfig {
    pub fn validate(
        &self,
        _query_ids: &[String],
        priority_queue_capacity: Option<usize>,
    ) -> anyhow::Result<()> {
        if self.github_token.trim().is_empty() {
            anyhow::bail!("`githubToken` must not be empty");
        }
        validate_url(&self.graphql_url).context("graphqlUrl")?;
        validate_url(&self.destination_event_url).context("destinationEventUrl")?;
        if self.status_field_name.trim().is_empty() {
            anyhow::bail!("`statusFieldName` must not be empty");
        }

        if self.request_timeout_ms == 0 {
            anyhow::bail!("`requestTimeoutMs` must be greater than 0");
        }
        if self.delivery_record_ttl_secs == 0 {
            anyhow::bail!("`deliveryRecordTtlSecs` must be greater than 0");
        }

        if matches!(priority_queue_capacity, Some(0)) {
            anyhow::bail!("`priorityQueueCapacity` must be greater than 0");
        }

        for project_id in &self.allowlisted_project_ids {
            validate_project_node_id(project_id).with_context(|| {
                format!("`allowlistedProjectIds` contains invalid project id '{project_id}'")
            })?;
        }

        for header_name in self.graphql_headers.keys() {
            reqwest::header::HeaderName::from_bytes(header_name.as_bytes())
                .with_context(|| format!("invalid GraphQL header name '{header_name}'"))?;
        }

        Ok(())
    }

    pub fn is_project_allowed(&self, project_node_id: &str) -> bool {
        self.allowlisted_project_ids.is_empty()
            || self
                .allowlisted_project_ids
                .iter()
                .any(|allowed| allowed == project_node_id)
    }
}

fn validate_url(raw_url: &str) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(raw_url).context("invalid URL")?;
    match url.scheme() {
        "http" | "https" => {}
        scheme => anyhow::bail!("unsupported URL scheme '{scheme}'; expected http or https"),
    }
    if url.host_str().is_none() {
        anyhow::bail!("URL must include a host");
    }
    Ok(())
}

pub(crate) fn validate_project_node_id(project_node_id: &str) -> anyhow::Result<()> {
    if !project_node_id.starts_with("PVT_") {
        anyhow::bail!("project node id must start with 'PVT_'");
    }
    if project_node_id.contains(char::is_whitespace) {
        anyhow::bail!("project node id must not contain whitespace");
    }
    Ok(())
}

pub(crate) fn validate_project_item_node_id(project_item_node_id: &str) -> anyhow::Result<()> {
    if !project_item_node_id.starts_with("PVTI_") {
        anyhow::bail!("project item node id must start with 'PVTI_'");
    }
    if project_item_node_id.contains(char::is_whitespace) {
        anyhow::bail!("project item node id must not contain whitespace");
    }
    Ok(())
}
