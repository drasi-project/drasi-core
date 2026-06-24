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

//! Descriptor for the MQTT reaction plugin.

use drasi_lib::reactions::Reaction;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

use crate::config::QueryConfig;
use crate::config::{MqttExtension, MqttProtocolVersion, MqttQoS, MqttTlsConfig};
use crate::MqttReactionBuilder;

/// DTO for MQTT QoS.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::mqtt::MqttQoS)]
#[serde(rename_all = "snake_case")]
pub enum MqttQoSDto {
    AtMostOnce,
    AtLeastOnce,
}

/// DTO for MQTT template specification.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::mqtt::MqttTemplateSpec)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttTemplateSpecDto {
    /// Handlebars payload template.
    #[serde(default)]
    pub template: String,

    /// Target topic (supports Handlebars templates).
    pub topic: String,

    /// MQTT QoS level.
    #[serde(default)]
    pub qos: Option<MqttQoSDto>,

    /// Retain flag.
    #[serde(default)]
    pub retain: bool,

    /// Publish zero-byte payload.
    #[serde(default)]
    pub empty_payload: bool,

    /// MQTT v5 message expiry interval in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_expiry_interval: Option<u32>,
}

/// DTO for per-query MQTT template configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::mqtt::MqttQueryConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttQueryConfigDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<MqttTemplateSpecDto>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<MqttTemplateSpecDto>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<MqttTemplateSpecDto>,
}

/// DTO for MQTT protocol version.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::mqtt::MqttProtocolVersion)]
#[serde(rename_all = "snake_case")]
pub enum MqttProtocolVersionDto {
    V5,
    V3_1_1,
}

/// DTO for MQTT TLS configuration.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::mqtt::MqttTlsConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttTlsConfigDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca: Option<Vec<u8>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub alpn: Option<Vec<Vec<u8>>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_auth: Option<(Vec<u8>, Vec<u8>)>,

    #[serde(default)]
    pub accept_invalid_certs: bool,
}

/// Configuration DTO for the MQTT reaction plugin.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = reaction::mqtt::MqttReactionConfig)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MqttReactionConfigDto {
    /// Broker URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub url: Option<ConfigValue<String>>,

    /// Optional explicit client id.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueString>)]
    pub client_id: Option<ConfigValue<String>>,

    /// MQTT protocol version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<MqttProtocolVersionDto>,

    /// Query-specific templates.
    #[serde(default)]
    pub routes: HashMap<String, MqttQueryConfigDto>,

    /// Default template configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<MqttQueryConfigDto>,

    /// TLS options.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<MqttTlsConfigDto>,

    /// Internal event channel capacity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_channel_capacity: Option<usize>,

    /// Maximum outgoing inflight QoS1 messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU16>)]
    pub max_inflight: Option<ConfigValue<u16>>,

    /// Keep alive in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub keep_alive: Option<ConfigValue<u64>>,

    /// Clean start flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueBool>)]
    pub clean_start: Option<ConfigValue<bool>>,

    /// Connection timeout in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub conn_timeout: Option<ConfigValue<u64>>,

    /// Session expiry interval (seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU32>)]
    pub session_expiry_interval: Option<ConfigValue<u32>>,
}

fn map_qos(dto: &MqttQoSDto) -> MqttQoS {
    match dto {
        MqttQoSDto::AtMostOnce => MqttQoS::AtMostOnce,
        MqttQoSDto::AtLeastOnce => MqttQoS::AtLeastOnce,
    }
}

fn map_template_spec(dto: &MqttTemplateSpecDto) -> crate::config::TemplateSpec<MqttExtension> {
    crate::config::TemplateSpec {
        template: dto.template.clone(),
        extension: MqttExtension {
            topic: dto.topic.clone(),
            qos: dto.qos.as_ref().map(map_qos).unwrap_or_default(),
            retain: dto.retain,
            empty_payload: dto.empty_payload,
            message_expiry_interval: dto.message_expiry_interval,
            slash_count: 0,
        },
    }
}

fn map_query_config(dto: &MqttQueryConfigDto) -> QueryConfig<MqttExtension> {
    QueryConfig {
        added: dto.added.as_ref().map(map_template_spec),
        updated: dto.updated.as_ref().map(map_template_spec),
        deleted: dto.deleted.as_ref().map(map_template_spec),
    }
}

fn map_protocol_version(dto: &MqttProtocolVersionDto) -> MqttProtocolVersion {
    match dto {
        MqttProtocolVersionDto::V5 => MqttProtocolVersion::V5,
        MqttProtocolVersionDto::V3_1_1 => MqttProtocolVersion::V3_1_1,
    }
}

fn map_tls_config(dto: &MqttTlsConfigDto) -> MqttTlsConfig {
    MqttTlsConfig {
        ca: dto.ca.clone(),
        alpn: dto.alpn.clone(),
        client_auth: dto.client_auth.clone(),
        accept_invalid_certs: dto.accept_invalid_certs,
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    MqttReactionConfigDto,
    MqttQueryConfigDto,
    MqttTemplateSpecDto,
    MqttQoSDto,
    MqttProtocolVersionDto,
    MqttTlsConfigDto,
)))]
struct MqttReactionSchemas;

/// Descriptor for the MQTT reaction plugin.
pub struct MqttReactionDescriptor;

#[async_trait]
impl ReactionPluginDescriptor for MqttReactionDescriptor {
    fn kind(&self) -> &str {
        "mqtt"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "reaction.mqtt.MqttReactionConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = MqttReactionSchemas::openapi();
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
        let dto: MqttReactionConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut builder = MqttReactionBuilder::new(id)
            .with_queries(query_ids)
            .with_auto_start(auto_start);

        if let Some(ref url) = dto.url {
            builder = builder.with_url(mapper.resolve_string(url).await?);
        }

        if let Some(ref client_id) = dto.client_id {
            builder = builder.with_client_id(mapper.resolve_string(client_id).await?);
        }

        if let Some(ref protocol_version) = dto.protocol_version {
            builder = builder.with_protocol_version(map_protocol_version(protocol_version));
        }

        if let Some(ref tls) = dto.tls {
            builder = builder.with_tls(map_tls_config(tls));
        }

        if let Some(event_channel_capacity) = dto.event_channel_capacity {
            builder = builder.with_event_channel_capacity(event_channel_capacity);
        }

        if let Some(ref max_inflight) = dto.max_inflight {
            builder = builder.with_max_inflight(mapper.resolve_typed(max_inflight).await?);
        }

        if let Some(ref keep_alive) = dto.keep_alive {
            builder = builder.with_keep_alive(mapper.resolve_typed(keep_alive).await?);
        }

        if let Some(ref clean_start) = dto.clean_start {
            builder = builder.with_clean_start(mapper.resolve_typed(clean_start).await?);
        }

        if let Some(ref conn_timeout) = dto.conn_timeout {
            builder = builder.with_conn_timeout(mapper.resolve_typed(conn_timeout).await?);
        }

        if let Some(ref session_expiry_interval) = dto.session_expiry_interval {
            builder = builder
                .with_session_expiry_interval(mapper.resolve_typed(session_expiry_interval).await?);
        }

        if let Some(ref default_template) = dto.default_template {
            builder = builder.with_default_template(map_query_config(default_template));
        }

        for (query_id, query_config) in &dto.routes {
            builder = builder.with_route(query_id, map_query_config(query_config));
        }

        let mut reaction = builder.build()?;
        reaction.base.set_raw_config(config_json.clone());

        Ok(Box::new(reaction))
    }
}
