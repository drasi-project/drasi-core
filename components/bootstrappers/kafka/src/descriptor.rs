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

//! Plugin descriptor for the Kafka bootstrap provider.

use crate::KafkaBootstrapProvider;
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use drasi_source_mapping::SourceMapping;
use std::collections::HashMap;
use utoipa::OpenApi;

/// Kafka bootstrap provider configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::kafka::KafkaBootstrapConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KafkaBootstrapConfigDto {
    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_servers: Option<ConfigValue<String>>,

    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<ConfigValue<String>>,

    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_label: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub mappings: Option<serde_json::Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub security_protocol: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub sasl_mechanism: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub sasl_username: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub sasl_password: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub additional_properties: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct KafkaSourceFallbackConfig {
    bootstrap_servers: Option<ConfigValue<String>>,
    topic: Option<ConfigValue<String>>,
    node_label: Option<ConfigValue<String>>,
    mappings: Option<serde_json::Value>,
    security_protocol: Option<ConfigValue<String>>,
    sasl_mechanism: Option<ConfigValue<String>>,
    sasl_username: Option<ConfigValue<String>>,
    sasl_password: Option<ConfigValue<String>>,
    #[serde(default)]
    additional_properties: HashMap<String, String>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(KafkaBootstrapConfigDto,)))]
struct KafkaBootstrapSchemas;

/// Plugin descriptor for the Kafka bootstrap provider.
pub struct KafkaBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for KafkaBootstrapDescriptor {
    fn kind(&self) -> &str {
        "kafka"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.kafka.KafkaBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = KafkaBootstrapSchemas::openapi();
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
        let dto: KafkaBootstrapConfigDto = serde_json::from_value(config_json.clone())?;
        let source_fallback: KafkaSourceFallbackConfig =
            serde_json::from_value(source_config_json.clone()).unwrap_or_default();
        let mapper = DtoMapper::new();

        let bootstrap_servers_cfg = dto
            .bootstrap_servers
            .as_ref()
            .or(source_fallback.bootstrap_servers.as_ref())
            .ok_or_else(|| anyhow::anyhow!("bootstrap_servers is required"))?;
        let topic_cfg = dto
            .topic
            .as_ref()
            .or(source_fallback.topic.as_ref())
            .ok_or_else(|| anyhow::anyhow!("topic is required"))?;
        let node_label_cfg = dto
            .node_label
            .as_ref()
            .or(source_fallback.node_label.as_ref())
            .ok_or_else(|| anyhow::anyhow!("node_label is required"))?;

        let mut builder = KafkaBootstrapProvider::builder()
            .bootstrap_servers(mapper.resolve_string(bootstrap_servers_cfg).await?)
            .topic(mapper.resolve_string(topic_cfg).await?)
            .node_label(mapper.resolve_string(node_label_cfg).await?);

        let mappings_json = dto.mappings.or(source_fallback.mappings);
        if let Some(value) = mappings_json {
            let mappings: Vec<SourceMapping> = serde_json::from_value(value)?;
            builder = builder.mappings(mappings);
        }

        let security_protocol_cfg = dto
            .security_protocol
            .as_ref()
            .or(source_fallback.security_protocol.as_ref());
        if let Some(cfg) = security_protocol_cfg {
            builder = builder.security_protocol(mapper.resolve_string(cfg).await?);
        }

        let sasl_mechanism_cfg = dto
            .sasl_mechanism
            .as_ref()
            .or(source_fallback.sasl_mechanism.as_ref());
        if let Some(cfg) = sasl_mechanism_cfg {
            builder = builder.sasl_mechanism(mapper.resolve_string(cfg).await?);
        }

        let sasl_username_cfg = dto
            .sasl_username
            .as_ref()
            .or(source_fallback.sasl_username.as_ref());
        if let Some(cfg) = sasl_username_cfg {
            builder = builder.sasl_username(mapper.resolve_string(cfg).await?);
        }

        let sasl_password_cfg = dto
            .sasl_password
            .as_ref()
            .or(source_fallback.sasl_password.as_ref());
        if let Some(cfg) = sasl_password_cfg {
            builder = builder.sasl_password(mapper.resolve_string(cfg).await?);
        }

        let mut additional_properties = source_fallback.additional_properties;
        additional_properties.extend(dto.additional_properties);
        for (key, value) in additional_properties {
            builder = builder.additional_property(key, value);
        }

        Ok(Box::new(builder.build()?))
    }
}
