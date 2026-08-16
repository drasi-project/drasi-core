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

use serde::{Deserialize, Serialize};

/// Default GitHub GraphQL API endpoint (github.com; override for GHE).
pub const DEFAULT_API_BASE_URL: &str = "https://api.github.com/graphql";
/// Default number of GraphQL requests allowed in flight at once.
pub const DEFAULT_MAX_CONCURRENCY: usize = 4;
/// Page size used for every paginated GraphQL connection.
pub const DEFAULT_PAGE_SIZE: u32 = 100;

/// Configuration for [`crate::GitHubWorkGraphBootstrapProvider`].
///
/// This bootstrapper owns all GitHub API access; the streaming
/// `drasi-source-github-workgraph` source is payload-only and never calls the
/// GitHub API. The `token` MUST be a read-only credential (a fine-grained PAT
/// with only `Issues: Read`, `Pull requests: Read`, and `Metadata: Read`
/// suffices) — this bootstrapper never writes to GitHub.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GitHubWorkGraphBootstrapConfig {
    /// The single GitHub organization login to enumerate.
    pub organization: String,
    /// A read-only GitHub token (PAT or GitHub App installation token).
    pub token: String,
    /// GraphQL API endpoint. Override for GitHub Enterprise Server.
    pub api_base_url: String,
    /// Upper bound on concurrently in-flight GraphQL requests, and on the
    /// number of repositories processed concurrently.
    pub max_concurrency: usize,
}

impl Default for GitHubWorkGraphBootstrapConfig {
    fn default() -> Self {
        Self {
            organization: String::new(),
            token: String::new(),
            api_base_url: DEFAULT_API_BASE_URL.to_string(),
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
        }
    }
}

impl GitHubWorkGraphBootstrapConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.organization.trim().is_empty(),
            "organization cannot be empty"
        );
        anyhow::ensure!(
            !self.organization.contains('/') && self.organization.trim() == self.organization,
            "organization must be one GitHub organization login"
        );
        anyhow::ensure!(!self.token.trim().is_empty(), "token cannot be empty");
        anyhow::ensure!(
            !self.api_base_url.trim().is_empty(),
            "api_base_url cannot be empty"
        );
        anyhow::ensure!(self.max_concurrency > 0, "max_concurrency must be > 0");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sane_values() {
        let config = GitHubWorkGraphBootstrapConfig::default();
        assert_eq!(config.api_base_url, DEFAULT_API_BASE_URL);
        assert_eq!(config.max_concurrency, DEFAULT_MAX_CONCURRENCY);
        assert!(config.validate().is_err(), "empty org/token must fail");
    }

    #[test]
    fn rejects_multi_segment_organization() {
        let config = GitHubWorkGraphBootstrapConfig {
            organization: "acme/eng".to_string(),
            token: "t".to_string(),
            ..GitHubWorkGraphBootstrapConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_zero_max_concurrency() {
        let config = GitHubWorkGraphBootstrapConfig {
            organization: "acme".to_string(),
            token: "t".to_string(),
            max_concurrency: 0,
            ..GitHubWorkGraphBootstrapConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn accepts_valid_config() {
        let config = GitHubWorkGraphBootstrapConfig {
            organization: "acme".to_string(),
            token: "t".to_string(),
            ..GitHubWorkGraphBootstrapConfig::default()
        };
        assert!(config.validate().is_ok());
    }
}
