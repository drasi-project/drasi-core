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

//! Descriptor for the RabbitMQ reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::{ExchangeType, PublishSpec, QueryPublishConfig, RabbitMQReactionBuilder};

/// DTO for a publish specification.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::rabbitmq::PublishSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PublishSpecDto {
    /// Routing key (supports Handlebars templates).
    pub routing_key: String,
    /// Custom AMQP headers (values support Handlebars templates).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Optional Handlebars template for message body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_template: Option<String>,
}

/// DTO for per-query publish configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::rabbitmq::QueryPublishConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct QueryPublishConfigDto {
    /// Publish specification for ADD operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<PublishSpecDto>,
    /// Publish specification for UPDATE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<PublishSpecDto>,
    /// Publish specification for DELETE operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<PublishSpecDto>,
}

/// Configuration DTO for the RabbitMQ reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::rabbitmq::RabbitMQReactionConfig)]
#[serde(rename_all = "camelCase")]
pub struct RabbitMQReactionConfigDto {
    /// AMQP connection string.
    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_string: Option<ConfigValue<String>>,
    /// Exchange name.
    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange_name: Option<ConfigValue<String>>,
    /// Exchange type (direct, topic, fanout, headers).
    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange_type: Option<ConfigValue<String>>,
    /// Whether the exchange is durable.
    #[schema(value_type = Option<ConfigValueBool>)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange_durable: Option<ConfigValue<bool>>,
    /// Whether messages are published as persistent.
    #[schema(value_type = Option<ConfigValueBool>)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_persistent: Option<ConfigValue<bool>>,
    /// Enable TLS for AMQPS connections.
    #[schema(value_type = Option<ConfigValueBool>)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_enabled: Option<ConfigValue<bool>>,
    /// Optional PEM certificate chain for TLS.
    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<ConfigValue<String>>,
    /// Optional PKCS#12 client identity (PFX).
    #[schema(value_type = Option<ConfigValueString>)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_key_path: Option<ConfigValue<String>>,
    /// Query-specific publish configuration.
    #[serde(default)]
    pub query_configs: HashMap<String, QueryPublishConfigDto>,
}

fn map_publish_spec(dto: &PublishSpecDto) -> PublishSpec {
    PublishSpec {
        routing_key: dto.routing_key.clone(),
        headers: dto.headers.clone(),
        body_template: dto.body_template.clone(),
    }
}

fn map_query_config(dto: &QueryPublishConfigDto) -> QueryPublishConfig {
    QueryPublishConfig {
        added: dto.added.as_ref().map(map_publish_spec),
        updated: dto.updated.as_ref().map(map_publish_spec),
        deleted: dto.deleted.as_ref().map(map_publish_spec),
    }
}

fn parse_exchange_type(value: &str) -> anyhow::Result<ExchangeType> {
    match value.to_lowercase().as_str() {
        "direct" => Ok(ExchangeType::Direct),
        "topic" => Ok(ExchangeType::Topic),
        "fanout" => Ok(ExchangeType::Fanout),
        "headers" => Ok(ExchangeType::Headers),
        _ => Err(anyhow::anyhow!(
            "Unsupported exchange_type '{value}'. Expected direct|topic|fanout|headers."
        )),
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(RabbitMQReactionConfigDto, QueryPublishConfigDto, PublishSpecDto,)))]
struct RabbitMQReactionSchemas;

/// Descriptor for the RabbitMQ reaction plugin.
pub struct RabbitMQReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for RabbitMQReactionDescriptor {
    fn kind(&self) -> &str {
        "rabbitmq"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.rabbitmq.RabbitMQReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = RabbitMQReactionSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_reaction(
        &self,
        id: &str,
        query_ids: Vec<String>,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn Reaction>> {
        let dto: RabbitMQReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();
        let defaults = crate::RabbitMQReactionConfig::default();

        let connection_string = match dto.connection_string.as_ref() {
            Some(value) => mapper.resolve_string(value)?,
            None => defaults.connection_string,
        };

        let exchange_name = match dto.exchange_name.as_ref() {
            Some(value) => mapper.resolve_string(value)?,
            None => defaults.exchange_name,
        };

        let exchange_type = match dto.exchange_type.as_ref() {
            Some(value) => parse_exchange_type(&mapper.resolve_string(value)?)?,
            None => defaults.exchange_type,
        };

        let mut builder = RabbitMQReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start)
            .with_connection_string(connection_string)
            .with_exchange(exchange_name, exchange_type);

        if let Some(ref exchange_durable) = dto.exchange_durable {
            builder = builder.with_exchange_durable(mapper.resolve_typed(exchange_durable)?);
        }

        if let Some(ref message_persistent) = dto.message_persistent {
            builder = builder.with_message_persistent(mapper.resolve_typed(message_persistent)?);
        }

        if let Some(ref tls_enabled) = dto.tls_enabled {
            if mapper.resolve_typed(tls_enabled)? {
                let tls_cert_path = match dto.tls_cert_path.as_ref() {
                    Some(value) => Some(mapper.resolve_string(value)?),
                    None => None,
                };
                let tls_key_path = match dto.tls_key_path.as_ref() {
                    Some(value) => Some(mapper.resolve_string(value)?),
                    None => None,
                };
                builder = builder.with_tls(tls_cert_path, tls_key_path);
            }
        }

        for (query_id, config) in &dto.query_configs {
            builder = builder.with_query_config(query_id, map_query_config(config));
        }

        let reaction = builder.build()?;
        Ok(Box::new(reaction))
    }
}
