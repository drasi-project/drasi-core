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

//! Configuration for the AHP source.

use anyhow::{anyhow, Result};
use drasi_ahp_common::sync::{
    DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_PAGE_SIZE, DEFAULT_REQUEST_TIMEOUT_MS,
};
use drasi_ahp_common::{AhpClientConfig, ResourceToken, SessionFilter};
use serde::{Deserialize, Serialize};

/// Default keepalive `ping` interval in milliseconds.
pub const DEFAULT_KEEPALIVE_INTERVAL_MS: u64 = 30_000;
/// Default initial reconnect delay in milliseconds.
pub const DEFAULT_RECONNECT_INITIAL_DELAY_MS: u64 = 1_000;
/// Default maximum reconnect delay in milliseconds.
pub const DEFAULT_RECONNECT_MAX_DELAY_MS: u64 = 30_000;
/// Default time `start()` waits for the initial snapshot, in milliseconds.
pub const DEFAULT_STARTUP_SYNC_TIMEOUT_MS: u64 = 15_000;

/// Configuration for [`crate::AhpSource`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AhpSourceConfig {
    /// WebSocket URL of the AHP host. Example: `ws://localhost:4321`.
    pub url: String,
    /// Optional bearer token sent as `Authorization: Bearer <token>` on connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_token: Option<String>,
    /// Client ID sent in `initialize`. Defaults to `drasi-<source id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Tokens pushed with the AHP `authenticate` command after connecting.
    #[serde(default)]
    pub resource_tokens: Vec<ResourceToken>,
    /// Only include sessions from these agent providers. Empty = all.
    #[serde(default)]
    pub providers: Vec<String>,
    /// Include archived sessions. Default `true`.
    pub include_archived: bool,
    /// Subscribe to each session channel for chats and input requests.
    /// Default `true`. When `false` only catalogue summaries are used.
    pub subscribe_sessions: bool,
    /// Mirror the `ahp-automations://` catalogue when supported. Default `true`.
    pub include_automations: bool,
    /// `listSessions` page size. Default 100.
    pub page_size: u32,
    /// Connect timeout in milliseconds. Default 10000.
    pub connect_timeout_ms: u64,
    /// Request timeout in milliseconds. Default 30000.
    pub request_timeout_ms: u64,
    /// Interval between liveness `ping`s in milliseconds; `0` disables. Default 30000.
    pub keepalive_interval_ms: u64,
    /// Initial reconnect backoff in milliseconds. Default 1000.
    pub reconnect_initial_delay_ms: u64,
    /// Maximum reconnect backoff in milliseconds. Default 30000.
    pub reconnect_max_delay_ms: u64,
    /// How long `start()` waits for the initial snapshot, in milliseconds.
    /// While it waits, that snapshot is a silent baseline (queries get initial
    /// data from the bootstrap provider). If the wait elapses, or is `0`, the
    /// first snapshot is emitted as inserts instead. Default 15000.
    pub startup_sync_timeout_ms: u64,
}

impl Default for AhpSourceConfig {
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
            keepalive_interval_ms: DEFAULT_KEEPALIVE_INTERVAL_MS,
            reconnect_initial_delay_ms: DEFAULT_RECONNECT_INITIAL_DELAY_MS,
            reconnect_max_delay_ms: DEFAULT_RECONNECT_MAX_DELAY_MS,
            startup_sync_timeout_ms: DEFAULT_STARTUP_SYNC_TIMEOUT_MS,
        }
    }
}

impl AhpSourceConfig {
    /// Connection settings for the shared AHP client.
    pub fn client_config(&self, source_id: &str) -> AhpClientConfig {
        AhpClientConfig {
            url: self.url.clone(),
            bearer_token: self.bearer_token.clone(),
            client_id: self
                .client_id
                .clone()
                .unwrap_or_else(|| format!("drasi-{source_id}")),
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
        self.client_config("validate").validate()?;
        if self.reconnect_initial_delay_ms == 0 {
            return Err(anyhow!("reconnectInitialDelayMs must be greater than zero"));
        }
        if self.reconnect_max_delay_ms < self.reconnect_initial_delay_ms {
            return Err(anyhow!(
                "reconnectMaxDelayMs must be >= reconnectInitialDelayMs"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        let config = AhpSourceConfig::default();
        assert!(config.validate().is_ok());
        assert!(config.include_archived);
        assert!(config.subscribe_sessions);
        assert_eq!(config.client_config("s1").client_id, "drasi-s1");
    }

    #[test]
    fn rejects_invalid_backoff_and_url() {
        let config = AhpSourceConfig {
            reconnect_max_delay_ms: 10,
            reconnect_initial_delay_ms: 100,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        let config = AhpSourceConfig {
            url: "http://nope".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn client_config_carries_filter() {
        let config = AhpSourceConfig {
            providers: vec!["copilot".into()],
            include_archived: false,
            client_id: Some("custom".into()),
            ..Default::default()
        };
        let client = config.client_config("s");
        assert_eq!(client.client_id, "custom");
        assert_eq!(client.filter.providers, vec!["copilot".to_string()]);
        assert!(!client.filter.include_archived);
    }
}
