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

//! AHP source plugin descriptor and configuration DTOs.

use crate::config::{
    DEFAULT_KEEPALIVE_INTERVAL_MS, DEFAULT_RECONNECT_INITIAL_DELAY_MS,
    DEFAULT_RECONNECT_MAX_DELAY_MS, DEFAULT_STARTUP_SYNC_TIMEOUT_MS,
};
use crate::{AhpSourceBuilder, AhpSourceConfig};
use drasi_ahp_common::sync::{
    DEFAULT_CONNECT_TIMEOUT_MS, DEFAULT_PAGE_SIZE, DEFAULT_REQUEST_TIMEOUT_MS,
};
use drasi_ahp_common::ResourceToken;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// Token pushed to the host with the AHP `authenticate` command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::ahp::ResourceToken)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceTokenDto {
    /// Protected resource identifier advertised by the agent.
    pub resource: String,
    /// Bearer token for the resource.
    #[schema(value_type = ConfigValueString)]
    pub token: ConfigValue<String>,
}

/// AHP source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::ahp::AhpSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AhpSourceConfigDto {
    /// WebSocket URL of the AHP host.
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
    #[schema(value_type = Vec<source::ahp::ResourceToken>)]
    pub resource_tokens: Vec<ResourceTokenDto>,
    /// Only include sessions of these agent providers.
    #[serde(default)]
    pub providers: Vec<String>,
    /// Include archived sessions.
    #[serde(default = "default_true")]
    pub include_archived: bool,
    /// Subscribe to session channels for chats and input requests.
    #[serde(default = "default_true")]
    pub subscribe_sessions: bool,
    /// Mirror the automation catalogue.
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
    /// Keepalive ping interval in milliseconds (0 disables).
    #[serde(default = "default_keepalive_interval_ms")]
    pub keepalive_interval_ms: u64,
    /// Initial reconnect backoff in milliseconds.
    #[serde(default = "default_reconnect_initial_delay_ms")]
    pub reconnect_initial_delay_ms: u64,
    /// Maximum reconnect backoff in milliseconds.
    #[serde(default = "default_reconnect_max_delay_ms")]
    pub reconnect_max_delay_ms: u64,
    /// How long start-up waits for the initial snapshot (0 = emit it as inserts).
    #[serde(default = "default_startup_sync_timeout_ms")]
    pub startup_sync_timeout_ms: u64,
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
fn default_keepalive_interval_ms() -> u64 {
    DEFAULT_KEEPALIVE_INTERVAL_MS
}
fn default_reconnect_initial_delay_ms() -> u64 {
    DEFAULT_RECONNECT_INITIAL_DELAY_MS
}
fn default_reconnect_max_delay_ms() -> u64 {
    DEFAULT_RECONNECT_MAX_DELAY_MS
}
fn default_startup_sync_timeout_ms() -> u64 {
    DEFAULT_STARTUP_SYNC_TIMEOUT_MS
}

impl AhpSourceConfigDto {
    /// Resolve `ConfigValue`s into a typed configuration.
    pub async fn resolve(&self, mapper: &DtoMapper) -> anyhow::Result<AhpSourceConfig> {
        let mut resource_tokens = Vec::with_capacity(self.resource_tokens.len());
        for token in &self.resource_tokens {
            resource_tokens.push(ResourceToken {
                resource: token.resource.clone(),
                token: mapper.resolve_string(&token.token).await?,
            });
        }
        Ok(AhpSourceConfig {
            url: mapper.resolve_string(&self.url).await?,
            bearer_token: mapper.resolve_optional_string(&self.bearer_token).await?,
            client_id: self.client_id.clone(),
            resource_tokens,
            providers: self.providers.clone(),
            include_archived: self.include_archived,
            subscribe_sessions: self.subscribe_sessions,
            include_automations: self.include_automations,
            page_size: self.page_size,
            connect_timeout_ms: self.connect_timeout_ms,
            request_timeout_ms: self.request_timeout_ms,
            keepalive_interval_ms: self.keepalive_interval_ms,
            reconnect_initial_delay_ms: self.reconnect_initial_delay_ms,
            reconnect_max_delay_ms: self.reconnect_max_delay_ms,
            startup_sync_timeout_ms: self.startup_sync_timeout_ms,
        })
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(AhpSourceConfigDto, ResourceTokenDto)))]
struct AhpSourceSchemas;

/// Descriptor for the AHP source plugin.
pub struct AhpSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for AhpSourceDescriptor {
    fn kind(&self) -> &str {
        "ahp"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.ahp.AhpSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        use drasi_plugin_sdk::schema_ui::SchemaUiAnnotator;

        let api = AhpSourceSchemas::openapi();
        let mut schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema");
        schemas["source.ahp.ResourceToken"]["properties"]["token"]["x-ui:widget"] =
            serde_json::Value::String("password".to_string());

        SchemaUiAnnotator::new(schemas, "source.ahp.AhpSourceConfig")
            .expect("root schema not found")
            .field("url", |f| {
                f.group("Connection")
                    .order(1)
                    .placeholder("ws://localhost:4321")
            })
            .field("bearerToken", |f| {
                f.group("Connection").order(2).widget("password")
            })
            .field("clientId", |f| f.group("Connection").order(3))
            .field("resourceTokens", |f| f.group("Authentication").order(1))
            .field("providers", |f| f.group("Selection").order(1))
            .field("includeArchived", |f| f.group("Selection").order(2))
            .field("subscribeSessions", |f| f.group("Selection").order(3))
            .field("includeAutomations", |f| f.group("Selection").order(4))
            .field("pageSize", |f| f.group("Advanced").collapsed(true))
            .field("connectTimeoutMs", |f| f.group("Advanced"))
            .field("requestTimeoutMs", |f| f.group("Advanced"))
            .field("keepaliveIntervalMs", |f| f.group("Advanced"))
            .field("reconnectInitialDelayMs", |f| f.group("Advanced"))
            .field("reconnectMaxDelayMs", |f| f.group("Advanced"))
            .field("startupSyncTimeoutMs", |f| f.group("Advanced"))
            .annotate()
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: AhpSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let config = dto.resolve(&DtoMapper::new()).await?;
        let mut source = AhpSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;
        source.base_mut().set_raw_config(config_json.clone());
        Ok(Box::new(source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::Source;
    use serde_json::json;

    #[tokio::test]
    async fn creates_source_with_defaults() {
        let config = json!({"url": "ws://localhost:9999"});
        let source = AhpSourceDescriptor
            .create_source("s", &config, false)
            .await
            .expect("source");
        assert_eq!(source.type_name(), "ahp");
        assert!(!source.auto_start());
        assert_eq!(source.properties()["url"], json!("ws://localhost:9999"));
    }

    #[tokio::test]
    async fn resolves_full_config() {
        let config = json!({
            "url": "wss://host/ahp",
            "bearerToken": "abc",
            "resourceTokens": [{"resource": "https://r", "token": "t"}],
            "providers": ["copilot"],
            "includeArchived": false,
            "keepaliveIntervalMs": 0
        });
        let dto: AhpSourceConfigDto = serde_json::from_value(config).expect("dto");
        let resolved = dto.resolve(&DtoMapper::new()).await.expect("resolved");
        assert_eq!(resolved.bearer_token.as_deref(), Some("abc"));
        assert_eq!(resolved.resource_tokens[0].token, "t");
        assert!(!resolved.include_archived);
        assert!(resolved.subscribe_sessions);
        assert_eq!(resolved.keepalive_interval_ms, 0);
    }

    #[test]
    fn rejects_unknown_fields() {
        let config = json!({"url": "ws://h", "bogus": 1});
        assert!(serde_json::from_value::<AhpSourceConfigDto>(config).is_err());
    }

    #[test]
    fn schema_is_valid_json() {
        let schema: serde_json::Value =
            serde_json::from_str(&AhpSourceDescriptor.config_schema_json()).expect("json");
        assert!(schema.get("source.ahp.AhpSourceConfig").is_some());
        assert!(schema.get("source.ahp.ResourceToken").is_some());
    }
}
