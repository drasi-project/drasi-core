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

//! RIS Live source plugin descriptor and configuration DTOs.

use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::config::StartFrom;
use crate::RisLiveSourceBuilder;

fn default_websocket_url() -> ConfigValue<String> {
    ConfigValue::Static("wss://ris-live.ripe.net/v1/ws/".to_string())
}

fn default_include_peer_state() -> ConfigValue<bool> {
    ConfigValue::Static(true)
}

fn default_reconnect_delay_secs() -> ConfigValue<u64> {
    ConfigValue::Static(5)
}

fn default_clear_state_on_start() -> ConfigValue<bool> {
    ConfigValue::Static(false)
}

/// RIS Live source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::rislive::RisLiveSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RisLiveSourceConfigDto {
    #[serde(default = "default_websocket_url")]
    pub websocket_url: ConfigValue<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_type: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefixes: Option<Vec<ConfigValue<String>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more_specific: Option<ConfigValue<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub less_specific: Option<ConfigValue<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require: Option<ConfigValue<String>>,
    #[serde(default = "default_include_peer_state")]
    pub include_peer_state: ConfigValue<bool>,
    #[serde(default = "default_reconnect_delay_secs")]
    pub reconnect_delay_secs: ConfigValue<u64>,
    #[serde(default = "default_clear_state_on_start")]
    pub clear_state_on_start: ConfigValue<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_from_beginning: Option<ConfigValue<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_from_now: Option<ConfigValue<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_from_timestamp: Option<ConfigValue<i64>>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(RisLiveSourceConfigDto)))]
struct RisLiveSourceSchemas;

/// Descriptor for the RIS Live source plugin.
pub struct RisLiveSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for RisLiveSourceDescriptor {
    fn kind(&self) -> &str {
        "ris-live"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.rislive.RisLiveSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = RisLiveSourceSchemas::openapi();
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
        let dto: RisLiveSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let websocket_url = mapper.resolve_string(&dto.websocket_url)?;
        let client_name = mapper.resolve_optional_string(&dto.client_name)?;
        let host = mapper.resolve_optional_string(&dto.host)?;
        let message_type = mapper.resolve_optional_string(&dto.message_type)?;
        let prefixes = dto
            .prefixes
            .as_deref()
            .map(|values| mapper.resolve_string_vec(values))
            .transpose()?;
        let more_specific = mapper.resolve_optional(&dto.more_specific)?;
        let less_specific = mapper.resolve_optional(&dto.less_specific)?;
        let path = mapper.resolve_optional_string(&dto.path)?;
        let peer = mapper.resolve_optional_string(&dto.peer)?;
        let require = mapper.resolve_optional_string(&dto.require)?;
        let include_peer_state = mapper.resolve_typed(&dto.include_peer_state)?;
        let reconnect_delay_secs = mapper.resolve_typed(&dto.reconnect_delay_secs)?;
        let clear_state_on_start = mapper.resolve_typed(&dto.clear_state_on_start)?;

        let start_from =
            if let Some(timestamp) = mapper.resolve_optional(&dto.start_from_timestamp)? {
                StartFrom::Timestamp {
                    timestamp_ms: timestamp,
                }
            } else if mapper
                .resolve_optional(&dto.start_from_beginning)?
                .unwrap_or(false)
            {
                StartFrom::Beginning
            } else {
                StartFrom::Now
            };

        let source = RisLiveSourceBuilder::new(id)
            .with_websocket_url(websocket_url)
            .with_optional_client_name(client_name)
            .with_optional_host(host)
            .with_optional_message_type(message_type)
            .with_optional_prefixes(prefixes)
            .with_optional_more_specific(more_specific)
            .with_optional_less_specific(less_specific)
            .with_optional_path(path)
            .with_optional_peer(peer)
            .with_optional_require(require)
            .with_include_peer_state(include_peer_state)
            .with_reconnect_delay_secs(reconnect_delay_secs)
            .with_clear_state_on_start(clear_state_on_start)
            .with_start_from(start_from)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
