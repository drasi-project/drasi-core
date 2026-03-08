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
pub const DEFAULT_DEEPBOOK_PACKAGE_ID: &str =
    "0x337f4f4f6567fcd778d5454f27c16c70e2f274cc6377ea6249ddf491482ef497";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(tag = "mode", content = "value", rename_all = "snake_case")]
pub enum StartPosition {
    #[default]
    Beginning,
    Now,
    Timestamp(i64),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SuiDeepBookBootstrapConfig {
    #[serde(default = "default_rpc_endpoint")]
    pub rpc_endpoint: String,
    #[serde(default = "default_deepbook_package_id")]
    pub deepbook_package_id: String,
    #[serde(default = "default_request_limit")]
    pub request_limit: u16,
    #[serde(default = "default_max_pages")]
    pub max_pages: u32,
    #[serde(default)]
    pub event_filters: Vec<String>,
    #[serde(default)]
    pub pools: Vec<String>,
    #[serde(default)]
    pub start_position: StartPosition,
    #[serde(default = "default_true")]
    pub enable_pool_nodes: bool,
    #[serde(default = "default_true")]
    pub enable_trader_nodes: bool,
    #[serde(default = "default_true")]
    pub enable_order_nodes: bool,
}

impl Default for SuiDeepBookBootstrapConfig {
    fn default() -> Self {
        Self {
            rpc_endpoint: default_rpc_endpoint(),
            deepbook_package_id: default_deepbook_package_id(),
            request_limit: default_request_limit(),
            max_pages: default_max_pages(),
            event_filters: Vec::new(),
            pools: Vec::new(),
            start_position: StartPosition::Beginning,
            enable_pool_nodes: true,
            enable_trader_nodes: true,
            enable_order_nodes: true,
        }
    }
}

impl SuiDeepBookBootstrapConfig {
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
        if self.max_pages == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: max_pages must be greater than 0"
            ));
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

fn default_request_limit() -> u16 {
    100
}

fn default_max_pages() -> u32 {
    10
}

fn default_true() -> bool {
    true
}
