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

//! Kafka source plugin descriptor and configuration DTOs.

use crate::{AutoOffsetReset, KafkaSource};
use drasi_plugin_sdk::prelude::*;
use drasi_source_mapping::SourceMapping;
use std::collections::HashMap;
use std::str::FromStr;
use utoipa::OpenApi;

/// Auto offset reset behavior DTO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = source::kafka::AutoOffsetReset)]
#[serde(rename_all = "lowercase")]
pub enum AutoOffsetResetDto {
    Earliest,
    Latest,
}

impl From<AutoOffsetResetDto> for AutoOffsetReset {
    fn from(value: AutoOffsetResetDto) -> Self {
        match value {
            AutoOffsetResetDto::Earliest => AutoOffsetReset::Earliest,
            AutoOffsetResetDto::Latest => AutoOffsetReset::Latest,
        }
    }
}

impl FromStr for AutoOffsetResetDto {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "earliest" => Ok(Self::Earliest),
            "latest" => Ok(Self::Latest),
            _ => Err(format!("Invalid auto offset reset: {s}")),
        }
    }
}

fn default_auto_offset_reset() -> ConfigValue<AutoOffsetResetDto> {
    ConfigValue::Static(AutoOffsetResetDto::Earliest)
}

/// Kafka source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::kafka::KafkaSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KafkaSourceConfigDto {
    #[schema(value_type = ConfigValueString)]
    pub bootstrap_servers: ConfigValue<String>,

    #[schema(value_type = ConfigValueString)]
    pub topic: ConfigValue<String>,

    #[schema(value_type = ConfigValueString)]
    pub group_id: ConfigValue<String>,

    #[schema(value_type = ConfigValueString)]
    pub node_label: ConfigValue<String>,

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

    #[serde(default = "default_auto_offset_reset")]
    #[schema(value_type = ConfigValue<source::kafka::AutoOffsetReset>)]
    pub auto_offset_reset: ConfigValue<AutoOffsetResetDto>,

    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub additional_properties: HashMap<String, String>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(KafkaSourceConfigDto, AutoOffsetResetDto,)))]
struct KafkaSourceSchemas;

/// Descriptor for the Kafka source plugin.
pub struct KafkaSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for KafkaSourceDescriptor {
    fn kind(&self) -> &str {
        "kafka"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.kafka.KafkaSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        use drasi_plugin_sdk::schema_ui::SchemaUiAnnotator;
        let api = KafkaSourceSchemas::openapi();
        let schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema");

        SchemaUiAnnotator::new(schemas, "source.kafka.KafkaSourceConfig")
            .expect("root schema not found")
            .field("bootstrapServers", |f| {
                f.group("Connection").order(1).placeholder("localhost:9092")
            })
            .field("topic", |f| {
                f.group("Connection").order(2).placeholder("orders")
            })
            .field("groupId", |f| {
                f.group("Connection").order(3).placeholder("drasi-orders")
            })
            .field("nodeLabel", |f| {
                f.group("Mapping").order(1).placeholder("Order")
            })
            .field("mappings", |f| f.group("Mapping").order(2))
            .field("autoOffsetReset", |f| f.group("Replay").order(1))
            .field("securityProtocol", |f| {
                f.group("Security").order(1).collapsed(true)
            })
            .field("saslMechanism", |f| f.group("Security").order(2))
            .field("saslUsername", |f| f.group("Security").order(3))
            .field("saslPassword", |f| {
                f.group("Security").order(4).widget("password")
            })
            .field("additionalProperties", |f| {
                f.group("Advanced").order(1).collapsed(true)
            })
            .annotate()
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        _auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: KafkaSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let bootstrap_servers = mapper.resolve_string(&dto.bootstrap_servers).await?;
        let topic = mapper.resolve_string(&dto.topic).await?;
        let group_id = mapper.resolve_string(&dto.group_id).await?;
        let node_label = mapper.resolve_string(&dto.node_label).await?;
        let auto_offset_reset: AutoOffsetReset = mapper
            .resolve_typed::<AutoOffsetResetDto>(&dto.auto_offset_reset)
            .await?
            .into();

        let mut builder = KafkaSource::builder(id)
            .bootstrap_servers(bootstrap_servers)
            .topic(topic)
            .group_id(group_id)
            .node_label(node_label)
            .auto_offset_reset(auto_offset_reset);

        if let Some(mappings_json) = dto.mappings {
            let mappings: Vec<SourceMapping> = serde_json::from_value(mappings_json)?;
            builder = builder.mappings(mappings);
        }

        if let Some(protocol) = dto.security_protocol.as_ref() {
            builder = builder.security_protocol(mapper.resolve_string(protocol).await?);
        }
        if let Some(mechanism) = dto.sasl_mechanism.as_ref() {
            builder = builder.sasl_mechanism(mapper.resolve_string(mechanism).await?);
        }
        if let Some(username) = dto.sasl_username.as_ref() {
            builder = builder.sasl_username(mapper.resolve_string(username).await?);
        }
        if let Some(password) = dto.sasl_password.as_ref() {
            builder = builder.sasl_password(mapper.resolve_string(password).await?);
        }

        for (key, value) in dto.additional_properties {
            builder = builder.additional_property(key, value);
        }

        let mut source = builder.build()?;
        source.base.set_raw_config(config_json.clone());

        Ok(Box::new(source))
    }
}
