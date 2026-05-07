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

pub const DEFAULT_SUI_MAINNET_RPC: &str = "https://fullnode.mainnet.sui.io:443";
pub const DEFAULT_SUI_MAINNET_GRPC: &str = "https://fullnode.mainnet.sui.io";
pub const DEFAULT_DEEPBOOK_PACKAGE_ID: &str =
    "0x337f4f4f6567fcd778d5454f27c16c70e2f274cc6377ea6249ddf491482ef497";

/// Transport mechanism for receiving events from the Sui blockchain.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    /// gRPC checkpoint streaming (recommended). Push-based, low latency (~300ms).
    /// Requires a Sui fullnode with gRPC support.
    #[default]
    Grpc,
    /// JSON-RPC polling (legacy). Pull-based, configurable poll interval.
    /// **Deprecated**: JSON-RPC will be deactivated July 2026.
    JsonRpc,
}

/// Where to start consuming events from the Sui blockchain.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub enum StartPosition {
    /// Start from the earliest available event on chain.
    Beginning,
    /// Start from the current chain tip; only process new events going forward.
    #[default]
    Now,
    /// Start from the beginning but skip events with `timestamp_ms` earlier than the given value (milliseconds since epoch).
    Timestamp(i64),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SuiDeepBookSourceConfig {
    /// Transport to use for event streaming. Default: gRPC.
    #[serde(default)]
    pub transport: Transport,
    /// Sui JSON-RPC endpoint URL. Used for JSON-RPC transport and pool-metadata lookups.
    #[serde(default = "default_rpc_endpoint")]
    pub rpc_endpoint: String,
    /// gRPC endpoint for checkpoint streaming. Defaults to Sui mainnet gRPC.
    /// Only used when transport is Grpc.
    pub grpc_endpoint: Option<String>,
    /// The DeepBook V3 package address on-chain (must start with `0x`).
    #[serde(default = "default_deepbook_package_id")]
    pub deepbook_package_id: String,
    /// Polling interval in milliseconds for JSON-RPC transport.
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// Maximum number of events to request per JSON-RPC page (1–1000).
    #[serde(default = "default_request_limit")]
    pub request_limit: u16,
    /// Event type filters. When non-empty, only events whose type suffix or substring matches are included.
    #[serde(default)]
    pub event_filters: Vec<String>,
    /// Pool address filters. When non-empty, only events from these pools are included.
    #[serde(default)]
    pub pools: Vec<String>,
    /// Where to start consuming events from the Sui blockchain.
    #[serde(default)]
    pub start_position: StartPosition,
    /// Whether to emit enrichment nodes for pools (pool metadata lookup).
    #[serde(default = "default_true")]
    pub enable_pool_nodes: bool,
    /// Whether to emit enrichment nodes for traders (sender address).
    #[serde(default = "default_true")]
    pub enable_trader_nodes: bool,
    /// Whether to emit enrichment nodes for orders (order_id extraction).
    #[serde(default = "default_true")]
    pub enable_order_nodes: bool,
    /// Number of recent historical events to fetch on startup (descending).
    /// When > 0, the source fetches the most recent N events before entering
    /// the forward-polling loop, giving dashboards immediate data.
    #[serde(default)]
    pub lookback_events: u16,
}

impl Default for SuiDeepBookSourceConfig {
    fn default() -> Self {
        Self {
            transport: Transport::default(),
            rpc_endpoint: default_rpc_endpoint(),
            grpc_endpoint: None,
            deepbook_package_id: default_deepbook_package_id(),
            poll_interval_ms: default_poll_interval_ms(),
            request_limit: default_request_limit(),
            event_filters: Vec::new(),
            pools: Vec::new(),
            start_position: StartPosition::Now,
            enable_pool_nodes: true,
            enable_trader_nodes: true,
            enable_order_nodes: true,
            lookback_events: 0,
        }
    }
}

impl SuiDeepBookSourceConfig {
    /// Returns the effective gRPC endpoint.
    /// If `grpc_endpoint` is set, uses that; otherwise falls back to the default
    /// gRPC endpoint (without explicit `:443` port, which is required for proper
    /// HTTP/2 `:authority` header handling with the Sui load balancer).
    pub fn effective_grpc_endpoint(&self) -> &str {
        self.grpc_endpoint
            .as_deref()
            .unwrap_or(DEFAULT_SUI_MAINNET_GRPC)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.rpc_endpoint.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: rpc_endpoint cannot be empty"
            ));
        }
        reqwest::Url::parse(&self.rpc_endpoint)
            .map_err(|e| anyhow::anyhow!("Validation error: invalid rpc_endpoint: {e}"))?;

        if self.deepbook_package_id.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: deepbook_package_id cannot be empty"
            ));
        }
        if !self.deepbook_package_id.starts_with("0x") {
            return Err(anyhow::anyhow!(
                "Validation error: deepbook_package_id must start with 0x"
            ));
        }
        if self.transport == Transport::JsonRpc {
            if self.poll_interval_ms == 0 {
                return Err(anyhow::anyhow!(
                    "Validation error: poll_interval_ms must be greater than 0"
                ));
            }
            if self.request_limit == 0 {
                return Err(anyhow::anyhow!(
                    "Validation error: request_limit must be greater than 0"
                ));
            }
            if self.request_limit > 1_000 {
                return Err(anyhow::anyhow!(
                    "Validation error: request_limit must be <= 1000"
                ));
            }
        }

        Ok(())
    }
}

fn default_rpc_endpoint() -> String {
    DEFAULT_SUI_MAINNET_RPC.to_string()
}

fn default_deepbook_package_id() -> String {
    DEFAULT_DEEPBOOK_PACKAGE_ID.to_string()
}

fn default_poll_interval_ms() -> u64 {
    2_000
}

fn default_request_limit() -> u16 {
    100
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> SuiDeepBookSourceConfig {
        SuiDeepBookSourceConfig::default()
    }

    #[test]
    fn test_validate_default_config_passes() {
        assert!(valid_config().validate().is_ok());
    }

    #[test]
    fn test_validate_empty_rpc_endpoint() {
        let mut config = valid_config();
        config.rpc_endpoint = "".to_string();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("rpc_endpoint cannot be empty"));
    }

    #[test]
    fn test_validate_invalid_rpc_endpoint_url() {
        let mut config = valid_config();
        config.rpc_endpoint = "not a url".to_string();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("invalid rpc_endpoint"));
    }

    #[test]
    fn test_validate_empty_package_id() {
        let mut config = valid_config();
        config.deepbook_package_id = "".to_string();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("deepbook_package_id cannot be empty"));
    }

    #[test]
    fn test_validate_package_id_missing_0x() {
        let mut config = valid_config();
        config.deepbook_package_id = "abc123".to_string();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("must start with 0x"));
    }

    #[test]
    fn test_validate_json_rpc_zero_poll_interval() {
        let mut config = valid_config();
        config.transport = Transport::JsonRpc;
        config.poll_interval_ms = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("poll_interval_ms must be greater than 0"));
    }

    #[test]
    fn test_validate_json_rpc_zero_request_limit() {
        let mut config = valid_config();
        config.transport = Transport::JsonRpc;
        config.request_limit = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("request_limit must be greater than 0"));
    }

    #[test]
    fn test_validate_json_rpc_request_limit_too_high() {
        let mut config = valid_config();
        config.transport = Transport::JsonRpc;
        config.request_limit = 1001;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("request_limit must be <= 1000"));
    }

    #[test]
    fn test_validate_grpc_skips_json_rpc_checks() {
        let mut config = valid_config();
        config.transport = Transport::Grpc;
        config.poll_interval_ms = 0; // would fail for JsonRpc
        config.request_limit = 5000; // would fail for JsonRpc
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_effective_grpc_endpoint_default() {
        let config = valid_config();
        assert_eq!(config.effective_grpc_endpoint(), DEFAULT_SUI_MAINNET_GRPC);
    }

    #[test]
    fn test_effective_grpc_endpoint_custom() {
        let mut config = valid_config();
        config.grpc_endpoint = Some("https://custom.node.io".to_string());
        assert_eq!(config.effective_grpc_endpoint(), "https://custom.node.io");
    }
}
