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

//! Configuration for the AHP bootstrap provider.

use anyhow::Result;
use drasi_ahp_common::sync::{
    DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_PAGE_SIZE, DEFAULT_REQUEST_TIMEOUT_MS,
};
use drasi_ahp_common::{AhpClientConfig, ResourceToken, SessionFilter};
use serde::{Deserialize, Serialize};

/// Configuration for [`crate::AhpBootstrapProvider`]. Selection settings
/// should match the AHP source so the bootstrap snapshot has the same shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AhpBootstrapConfig {
    /// WebSocket URL of the AHP host.
    pub url: String,
    /// Optional bearer token sent as `Authorization: Bearer <token>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// Client ID sent in `initialize`. Defaults to `drasi-<source id>-bootstrap`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Tokens pushed with the AHP `authenticate` command.
    #[serde(default)]
    pub resource_tokens: Vec<ResourceToken>,
    /// Only include sessions of these agent providers. Empty = all.
    #[serde(default)]
    pub providers: Vec<String>,
    /// Include archived sessions.
    pub include_archived: bool,
    /// Read each session channel for chats and input requests.
    pub subscribe_sessions: bool,
    /// Include the automation catalogue.
    pub include_automations: bool,
    /// `listSessions` page size.
    pub page_size: u32,
    /// Connect timeout in milliseconds.
    pub connect_timeout_ms: u64,
    /// Request timeout in milliseconds.
    pub request_timeout_ms: u64,
}

impl Default for AhpBootstrapConfig {
    fn default() -> Self {
        Self {
            url: "ws://localhost:4321".to_string(),
            bearer_token: None,
            client_id: None,
            resource_tokens: Vec::new(),
            providers: Vec::new(),
            include_archived: true,
            subscribe_sessions: true,
            include_automations: true,
            page_size: DEFAULT_PAGE_SIZE,
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            request_timeout_ms: DEFAULT_REQUEST_TIMEOUT_MS,
        }
    }
}

impl AhpBootstrapConfig {
    /// Connection settings for the shared AHP client.
    pub fn client_config(&self, source_id: &str) -> AhpClientConfig {
        AhpClientConfig {
            url: self.url.clone(),
            bearer_token: self.bearer_token.clone(),
            client_id: self
                .client_id
                .clone()
                .unwrap_or_else(|| format!("drasi-{source_id}-bootstrap")),
            resource_tokens: self.resource_tokens.clone(),
            filter: SessionFilter {
                providers: self.providers.clone(),
                include_archived: self.include_archived,
            },
            subscribe_sessions: self.subscribe_sessions,
            include_automations: self.include_automations,
            page_size: self.page_size,
            connect_timeout_ms: self.connect_timeout_ms,
            request_timeout_ms: self.request_timeout_ms,
        }
    }

    /// Validate the configuration.
    pub fn validate(&self) -> Result<()> {
        self.client_config("validate").validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let config = AhpBootstrapConfig::default();
        assert!(config.validate().is_ok());
        assert_eq!(config.client_config("s").client_id, "drasi-s-bootstrap");
    }

    #[test]
    fn rejects_bad_url() {
        let config = AhpBootstrapConfig {
            url: "ftp://x".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }
}
