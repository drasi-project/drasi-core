// Copyright 2025 The Drasi Authors.
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

//! Configuration types for the Hyperliquid source.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Initial cursor behavior for the Hyperliquid source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InitialCursor {
    /// Start streaming from the earliest data available (no filtering).
    StartFromBeginning,
    /// Start streaming from the moment the source starts (default).
    StartFromNow,
    /// Start streaming from a specific timestamp (milliseconds since epoch).
    StartFromTimestamp { timestamp: i64 },
}

impl Default for InitialCursor {
    fn default() -> Self {
        Self::StartFromNow
    }
}

impl InitialCursor {
    /// Resolve the timestamp filter for streaming.
    pub fn start_timestamp(&self) -> Option<i64> {
        match self {
            Self::StartFromBeginning => None,
            Self::StartFromNow => Some(chrono::Utc::now().timestamp_millis()),
            Self::StartFromTimestamp { timestamp } => Some(*timestamp),
        }
    }
}

/// Hyperliquid environment selection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HyperliquidNetwork {
    Mainnet,
    Testnet,
    Custom { rest_url: String, ws_url: String },
}

impl Default for HyperliquidNetwork {
    fn default() -> Self {
        Self::Mainnet
    }
}

impl HyperliquidNetwork {
    pub fn rest_url(&self) -> String {
        match self {
            Self::Mainnet => "https://api.hyperliquid.xyz/info".to_string(),
            Self::Testnet => "https://api.hyperliquid-testnet.xyz/info".to_string(),
            Self::Custom { rest_url, .. } => rest_url.clone(),
        }
    }

    pub fn ws_url(&self) -> String {
        match self {
            Self::Mainnet => "wss://api.hyperliquid.xyz/ws".to_string(),
            Self::Testnet => "wss://api.hyperliquid-testnet.xyz/ws".to_string(),
            Self::Custom { ws_url, .. } => ws_url.clone(),
        }
    }
}

/// Coin selection strategy for per-coin subscriptions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoinSelection {
    /// Subscribe to a specific list of coins.
    Specific { coins: Vec<String> },
    /// Subscribe to all available coins (discovered via metadata).
    All,
}

impl Default for CoinSelection {
    fn default() -> Self {
        Self::All
    }
}

impl CoinSelection {
    pub fn coins(&self) -> Option<&[String]> {
        match self {
            Self::Specific { coins } => Some(coins.as_slice()),
            Self::All => None,
        }
    }
}

/// Hyperliquid source configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HyperliquidSourceConfig {
    pub network: HyperliquidNetwork,
    pub coins: CoinSelection,
    pub enable_trades: bool,
    pub enable_order_book: bool,
    pub enable_mid_prices: bool,
    pub enable_funding_rates: bool,
    pub enable_liquidations: bool,
    pub funding_poll_interval_secs: u64,
    pub initial_cursor: InitialCursor,
}

impl Default for HyperliquidSourceConfig {
    fn default() -> Self {
        Self {
            network: HyperliquidNetwork::Mainnet,
            coins: CoinSelection::All,
            enable_trades: false,
            enable_order_book: false,
            enable_mid_prices: true,
            enable_funding_rates: false,
            enable_liquidations: false,
            funding_poll_interval_secs: 60,
            initial_cursor: InitialCursor::StartFromNow,
        }
    }
}

impl HyperliquidSourceConfig {
    /// Validate configuration values.
    pub fn validate(&self) -> Result<()> {
        if let CoinSelection::Specific { coins } = &self.coins {
            if coins.is_empty() {
                return Err(anyhow!(
                    "Validation error: coins cannot be empty when using Specific selection"
                ));
            }
        }

        if self.funding_poll_interval_secs == 0 {
            return Err(anyhow!(
                "Validation error: funding_poll_interval_secs must be greater than 0"
            ));
        }

        if !self.enable_trades
            && !self.enable_order_book
            && !self.enable_mid_prices
            && !self.enable_funding_rates
            && !self.enable_liquidations
        {
            return Err(anyhow!(
                "Validation error: at least one data channel must be enabled"
            ));
        }

        Ok(())
    }
}
