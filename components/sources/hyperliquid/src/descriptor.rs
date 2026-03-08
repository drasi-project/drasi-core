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

//! Hyperliquid source plugin descriptor and configuration DTOs.

use crate::{CoinSelection, HyperliquidNetwork, HyperliquidSourceBuilder, InitialCursor};
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// Hyperliquid source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::hyperliquid::HyperliquidSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HyperliquidSourceConfigDto {
    #[serde(default)]
    #[schema(value_type = source::hyperliquid::HyperliquidNetwork)]
    pub network: HyperliquidNetworkDto,
    #[serde(default)]
    #[schema(value_type = source::hyperliquid::CoinSelection)]
    pub coins: CoinSelectionDto,
    #[serde(default = "default_bool_false")]
    pub enable_trades: ConfigValue<bool>,
    #[serde(default = "default_bool_false")]
    pub enable_order_book: ConfigValue<bool>,
    #[serde(default = "default_bool_true")]
    pub enable_mid_prices: ConfigValue<bool>,
    #[serde(default = "default_bool_false")]
    pub enable_funding_rates: ConfigValue<bool>,
    #[serde(default = "default_bool_false")]
    pub enable_liquidations: ConfigValue<bool>,
    #[serde(default = "default_poll_interval")]
    pub funding_poll_interval_secs: ConfigValue<u64>,
    #[serde(default)]
    #[schema(value_type = source::hyperliquid::InitialCursor)]
    pub initial_cursor: InitialCursorDto,
}

fn default_bool_false() -> ConfigValue<bool> {
    ConfigValue::Static(false)
}

fn default_bool_true() -> ConfigValue<bool> {
    ConfigValue::Static(true)
}

fn default_poll_interval() -> ConfigValue<u64> {
    ConfigValue::Static(60)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::hyperliquid::HyperliquidNetwork)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HyperliquidNetworkDto {
    Mainnet,
    Testnet,
    Custom {
        rest_url: ConfigValue<String>,
        ws_url: ConfigValue<String>,
    },
}

impl Default for HyperliquidNetworkDto {
    fn default() -> Self {
        Self::Mainnet
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::hyperliquid::CoinSelection)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CoinSelectionDto {
    Specific { coins: Vec<String> },
    All,
}

impl Default for CoinSelectionDto {
    fn default() -> Self {
        Self::All
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::hyperliquid::InitialCursor)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InitialCursorDto {
    StartFromBeginning,
    StartFromNow,
    StartFromTimestamp { timestamp: ConfigValue<i64> },
}

impl Default for InitialCursorDto {
    fn default() -> Self {
        Self::StartFromNow
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    HyperliquidSourceConfigDto,
    HyperliquidNetworkDto,
    CoinSelectionDto,
    InitialCursorDto,
)))]
struct HyperliquidSchemas;

/// Descriptor for the Hyperliquid source plugin.
pub struct HyperliquidSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for HyperliquidSourceDescriptor {
    fn kind(&self) -> &str {
        "hyperliquid"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.hyperliquid.HyperliquidSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = HyperliquidSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: HyperliquidSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let network = match dto.network {
            HyperliquidNetworkDto::Mainnet => HyperliquidNetwork::Mainnet,
            HyperliquidNetworkDto::Testnet => HyperliquidNetwork::Testnet,
            HyperliquidNetworkDto::Custom { rest_url, ws_url } => {
                let rest_url = mapper.resolve_typed(&rest_url)?;
                let ws_url = mapper.resolve_typed(&ws_url)?;
                HyperliquidNetwork::Custom { rest_url, ws_url }
            }
        };

        let coins = match dto.coins {
            CoinSelectionDto::Specific { coins } => CoinSelection::Specific { coins },
            CoinSelectionDto::All => CoinSelection::All,
        };

        let initial_cursor = match dto.initial_cursor {
            InitialCursorDto::StartFromBeginning => InitialCursor::StartFromBeginning,
            InitialCursorDto::StartFromNow => InitialCursor::StartFromNow,
            InitialCursorDto::StartFromTimestamp { timestamp } => {
                let ts = mapper.resolve_typed(&timestamp)?;
                InitialCursor::StartFromTimestamp { timestamp: ts }
            }
        };

        let mut builder = HyperliquidSourceBuilder::new(id)
            .with_network(network)
            .with_auto_start(auto_start)
            .with_funding_poll_interval_secs(mapper.resolve_typed(&dto.funding_poll_interval_secs)?)
            .with_mid_prices(mapper.resolve_typed(&dto.enable_mid_prices)?)
            .with_trades(mapper.resolve_typed(&dto.enable_trades)?)
            .with_order_book(mapper.resolve_typed(&dto.enable_order_book)?)
            .with_liquidations(mapper.resolve_typed(&dto.enable_liquidations)?)
            .with_funding_rates(mapper.resolve_typed(&dto.enable_funding_rates)?);

        builder = match coins {
            CoinSelection::Specific { coins } => builder.with_coins(coins),
            CoinSelection::All => builder.with_all_coins(),
        };

        builder = match initial_cursor {
            InitialCursor::StartFromBeginning => builder.start_from_beginning(),
            InitialCursor::StartFromNow => builder.start_from_now(),
            InitialCursor::StartFromTimestamp { timestamp } => {
                builder.start_from_timestamp(timestamp)
            }
        };

        let source = builder.build()?;
        Ok(Box::new(source))
    }
}
