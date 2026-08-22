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

use anyhow::{bail, ensure, Context, Result};
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const DEFAULT_API_URL: &str = "https://api.github.com";
pub const DEFAULT_USER_AGENT: &str = "drasi-github-workgraph-dispatcher";
pub const DEFAULT_API_VERSION: &str = "2022-11-28";

fn default_api_url() -> String {
    DEFAULT_API_URL.to_string()
}

fn default_user_agent() -> String {
    DEFAULT_USER_AGENT.to_string()
}

fn default_api_version() -> String {
    DEFAULT_API_VERSION.to_string()
}

fn default_request_timeout_ms() -> u64 {
    30_000
}

fn default_max_attempts() -> u32 {
    4
}

fn default_initial_retry_delay_ms() -> u64 {
    500
}

fn default_priority_queue_capacity() -> usize {
    10_000
}

/// Resolved runtime configuration. The token and header values are never logged.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubWorkGraphDispatcherConfig {
    #[serde(default = "default_api_url")]
    pub api_url: String,
    #[serde(skip_serializing)]
    pub token: String,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    #[serde(default = "default_api_version")]
    pub api_version: String,
    #[serde(default, skip_serializing)]
    pub headers: HashMap<String, String>,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_initial_retry_delay_ms")]
    pub initial_retry_delay_ms: u64,
    #[serde(default = "default_priority_queue_capacity")]
    pub priority_queue_capacity: usize,
}

impl std::fmt::Debug for GitHubWorkGraphDispatcherConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GitHubWorkGraphDispatcherConfig")
            .field("api_url", &self.api_url)
            .field("token", &"<redacted>")
            .field("user_agent", &self.user_agent)
            .field("api_version", &self.api_version)
            .field(
                "headers",
                &format_args!("<{} configured>", self.headers.len()),
            )
            .field("request_timeout_ms", &self.request_timeout_ms)
            .field("max_attempts", &self.max_attempts)
            .field("initial_retry_delay_ms", &self.initial_retry_delay_ms)
            .field("priority_queue_capacity", &self.priority_queue_capacity)
            .finish()
    }
}

impl Default for GitHubWorkGraphDispatcherConfig {
    fn default() -> Self {
        Self {
            api_url: default_api_url(),
            token: String::new(),
            user_agent: default_user_agent(),
            api_version: default_api_version(),
            headers: HashMap::new(),
            request_timeout_ms: default_request_timeout_ms(),
            max_attempts: default_max_attempts(),
            initial_retry_delay_ms: default_initial_retry_delay_ms(),
            priority_queue_capacity: default_priority_queue_capacity(),
        }
    }
}

impl GitHubWorkGraphDispatcherConfig {
    pub fn normalized_api_url(&self) -> String {
        self.api_url.trim_end_matches('/').to_string()
    }

    pub fn validate(&self, queries: &[String]) -> Result<()> {
        ensure!(
            queries.len() == 1 && !queries[0].trim().is_empty(),
            "github-workgraph-dispatcher requires exactly one non-empty capacity query"
        );
        ensure!(
            !self.token.is_empty()
                && !self.token.chars().any(char::is_control)
                && self.token.trim() == self.token,
            "token must resolve to a non-empty value without control or surrounding whitespace"
        );

        let url = Url::parse(&self.api_url).context("apiUrl must be an absolute URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "apiUrl must be an absolute HTTP(S) URL without credentials, a query, or a fragment"
        );
        ensure!(
            !self.user_agent.trim().is_empty(),
            "userAgent must not be empty"
        );
        HeaderValue::from_str(&self.user_agent).context("userAgent is not a valid header value")?;
        ensure!(
            !self.api_version.trim().is_empty(),
            "apiVersion must not be empty"
        );
        HeaderValue::from_str(&self.api_version)
            .context("apiVersion is not a valid header value")?;
        ensure!(
            (1..=120_000).contains(&self.request_timeout_ms),
            "requestTimeoutMs must be between 1 and 120000"
        );
        ensure!(
            (1..=10).contains(&self.max_attempts),
            "maxAttempts must be between 1 and 10"
        );
        ensure!(
            (1..=60_000).contains(&self.initial_retry_delay_ms),
            "initialRetryDelayMs must be between 1 and 60000"
        );
        ensure!(
            (1..=100_000).contains(&self.priority_queue_capacity),
            "priorityQueueCapacity must be between 1 and 100000"
        );

        for (name, value) in &self.headers {
            let header = HeaderName::from_bytes(name.as_bytes())
                .context("invalid configured header name")?;
            HeaderValue::from_str(value)
                .with_context(|| format!("configured header '{name}' has an invalid value"))?;
            if matches!(
                header.as_str(),
                "authorization"
                    | "accept"
                    | "content-type"
                    | "user-agent"
                    | "x-github-api-version"
                    | "host"
                    | "content-length"
                    | "transfer-encoding"
                    | "connection"
            ) {
                bail!("configured header '{name}' may not override a dispatcher-owned header");
            }
        }
        Ok(())
    }
}
