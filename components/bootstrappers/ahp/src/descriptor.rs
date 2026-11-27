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

//! Plugin descriptor for the AHP bootstrap provider.

use drasi_ahp_common::sync::{
    DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_PAGE_SIZE, DEFAULT_REQUEST_TIMEOUT_MS,
};
use drasi_ahp_common::ResourceToken;
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::{AhpBootstrapConfig, AhpBootstrapProvider};

/// Keys of the bootstrap DTO. When the bootstrap config omits them, they are
/// inherited from the AHP source configuration so an empty `{}` config works.
const INHERITED_KEYS: &[&str] = &[
    "url",
    "bearerToken",
    "resourceTokens",
    "providers",
    "includeArchived",
    "subscribeSessions",
    "includeAutomations",
    "pageSize",
    "connectTimeoutMs",
    "requestTimeoutMs",
];

/// Token pushed to the host with the AHP `authenticate` command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::ahp::ResourceToken)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceTokenDto {
    /// Protected resource identifier advertised by the agent.
    pub resource: String,
    /// Bearer token for the resource.
    #[schema(value_type = ConfigValueString)]
    pub token: ConfigValue<String>,
}

/// AHP bootstrap provider configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::ahp::AhpBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AhpBootstrapConfigDto {
    /// WebSocket URL of the AHP host. Inherited from the source when omitted.
    #[schema(value_type = ConfigValueString)]
    pub url: ConfigValue<String>,
    /// Bearer token for the WebSocket upgrade request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub bearer_token: Option<ConfigValue<String>>,
    /// Client ID sent in `initialize`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Tokens pushed with the AHP `authenticate` command.
    #[serde(default)]
    #[schema(value_type = Vec<bootstrap::ahp::ResourceToken>)]
    pub resource_tokens: Vec<ResourceTokenDto>,
    /// Only include sessions of these agent providers.
    #[serde(default)]
    pub providers: Vec<String>,
    /// Include archived sessions.
    #[serde(default = "default_true")]
    pub include_archived: bool,
    /// Read session channels for chats and input requests.
    #[serde(default = "default_true")]
    pub subscribe_sessions: bool,
    /// Include the automation catalogue.
    #[serde(default = "default_true")]
    pub include_automations: bool,
    /// `listSessions` page size.
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Connect timeout in milliseconds.
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// Request timeout in milliseconds.
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
}

fn default_true() -> bool {
    true
}
fn default_page_size() -> u32 {
    DEFAULT_PAGE_SIZE
}
fn default_connect_timeout_ms() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_MS
}
fn default_request_timeout_ms() -> u64 {
    DEFAULT_REQUEST_TIMEOUT_MS
}

/// Overlay the bootstrap config on the inheritable keys of the source config.
fn merge_with_source(
    config_json: &serde_json::Value,
    source_config_json: &serde_json::Value,
) -> serde_json::Value {
    let mut merged = serde_json::Map::new();
    if let Some(source) = source_config_json.as_object() {
        for key in INHERITED_KEYS {
            if let Some(value) = source.get(*key) {
                merged.insert((*key).to_string(), value.clone());
            }
        }
    }
    if let Some(own) = config_json.as_object() {
        for (key, value) in own {
            merged.insert(key.clone(), value.clone());
        }
    }
    serde_json::Value::Object(merged)
}

#[derive(OpenApi)]
#[openapi(components(schemas(AhpBootstrapConfigDto, ResourceTokenDto)))]
struct AhpBootstrapSchemas;

/// Plugin descriptor for the AHP bootstrap provider.
pub struct AhpBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for AhpBootstrapDescriptor {
    fn kind(&self) -> &str {
        "ahp"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.ahp.AhpBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = AhpBootstrapSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_bootstrap_provider(
        &self,
        config_json: &serde_json::Value,
        source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let merged = merge_with_source(config_json, source_config_json);
        let dto: AhpBootstrapConfigDto = serde_json::from_value(merged)?;
        let mapper = DtoMapper::new();
        let mut resource_tokens = Vec::with_capacity(dto.resource_tokens.len());
        for token in &dto.resource_tokens {
            resource_tokens.push(ResourceToken {
                resource: token.resource.clone(),
                token: mapper.resolve_string(&token.token).await?,
            });
        }
        let config = AhpBootstrapConfig {
            url: mapper.resolve_string(&dto.url).await?,
            bearer_token: mapper.resolve_optional_string(&dto.bearer_token).await?,
            client_id: dto.client_id,
            resource_tokens,
            providers: dto.providers,
            include_archived: dto.include_archived,
            subscribe_sessions: dto.subscribe_sessions,
            include_automations: dto.include_automations,
            page_size: dto.page_size,
            connect_timeout_ms: dto.connect_timeout_ms,
            request_timeout_ms: dto.request_timeout_ms,
        };
        config.validate()?;
        Ok(Box::new(AhpBootstrapProvider::new(config)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inherits_connection_settings_from_source() {
        let source = json!({
            "url": "ws://host:1",
            "bearerToken": "secret",
            "providers": ["copilot"],
            "keepaliveIntervalMs": 5
        });
        let merged = merge_with_source(&json!({"pageSize": 5}), &source);
        let dto: AhpBootstrapConfigDto = serde_json::from_value(merged).expect("dto");
        assert_eq!(dto.url, ConfigValue::Static("ws://host:1".to_string()));
        assert_eq!(dto.providers, vec!["copilot".to_string()]);
        assert_eq!(dto.page_size, 5);
    }

    #[test]
    fn own_config_wins() {
        let merged = merge_with_source(
            &json!({"url": "ws://own:2"}),
            &json!({"url": "ws://source:1"}),
        );
        assert_eq!(merged["url"], json!("ws://own:2"));
    }

    #[tokio::test]
    async fn creates_provider() {
        let provider = AhpBootstrapDescriptor
            .create_bootstrap_provider(&json!({}), &json!({"url": "ws://h:1"}))
            .await;
        assert!(provider.is_ok());
        let missing_url = AhpBootstrapDescriptor
            .create_bootstrap_provider(&json!({}), &json!({}))
            .await;
        assert!(missing_url.is_err());
    }
}
