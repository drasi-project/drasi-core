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

//! MQTT source plugin descriptor and configuration DTOs.

use crate::config::{
    InjectId, MappingEntity, MappingMode, MappingNode, MappingProperties, MappingRelation,
    MqttConnectProperties, MqttQoS, MqttSourceConfig, MqttSubscribeProperties, MqttTopicConfig,
    MqttTransportMode, TopicMapping,
};
use crate::MqttSourceBuilder;
use anyhow::anyhow;
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

fn default_mqtt_host() -> ConfigValue<String> {
    ConfigValue::Static("localhost".to_string())
}

fn default_mqtt_port() -> ConfigValue<u16> {
    ConfigValue::Static(1883)
}

fn default_event_channel_capacity_dto() -> ConfigValue<usize> {
    ConfigValue::Static(20)
}

fn default_max_retries_dto() -> Option<ConfigValue<u32>> {
    Some(ConfigValue::Static(8))
}

fn default_base_retry_delay_secs_dto() -> Option<ConfigValue<u64>> {
    Some(ConfigValue::Static(1))
}

/// DTO for topic-level MQTT subscription configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mqtt::MqttTopicConfig)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct MqttTopicConfigDto {
    pub topic: String,
    /// QoS as integer value: 0, 1, or 2.
    pub qos: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mqtt::MqttConnectProperties)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct MqttConnectPropertiesDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_expiry_interval: Option<ConfigValue<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receive_maximum: Option<ConfigValue<u16>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_packet_size: Option<ConfigValue<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic_alias_max: Option<ConfigValue<u16>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_response_info: Option<ConfigValue<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_problem_info: Option<ConfigValue<u8>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_properties: Vec<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication_method: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication_data: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mqtt::MqttSubscribeProperties)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct MqttSubscribePropertiesDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<ConfigValue<usize>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_properties: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mqtt::MqttTlsTransportConfig)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct MqttTlsTransportConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_path: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alpn: Option<Vec<Vec<u8>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_auth: Option<(Vec<u8>, Vec<u8>)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert_path: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_path: Option<ConfigValue<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mqtt::MqttTransportMode)]
#[serde(
    rename_all = "lowercase",
    tag = "mode",
    content = "config",
    deny_unknown_fields
)]
pub enum MqttTransportModeDto {
    #[serde(rename = "tcp")]
    Tcp,
    #[serde(rename = "tls")]
    Tls(MqttTlsTransportConfigDto),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mqtt::MappingEntity)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct MappingEntityDto {
    pub label: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = source::mqtt::MappingMode)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum MappingModeDto {
    PayloadAsField,
    PayloadSpread,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[schema(as = source::mqtt::InjectId)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum InjectIdDto {
    True,
    False,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mqtt::MappingProperties)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct MappingPropertiesDto {
    pub mode: MappingModeDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inject_id: Option<InjectIdDto>,
    /// JSON object mapping topic variables to graph properties.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inject: Vec<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mqtt::MappingNode)]
#[serde(deny_unknown_fields)]
pub struct MappingNodeDto {
    pub label: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mqtt::MappingRelation)]
#[serde(deny_unknown_fields)]
pub struct MappingRelationDto {
    pub label: String,
    pub from: String,
    pub to: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mqtt::TopicMapping)]
#[serde(deny_unknown_fields)]
pub struct TopicMappingDto {
    pub pattern: String,
    pub entity: MappingEntityDto,
    pub properties: MappingPropertiesDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<MappingNodeDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<MappingRelationDto>,
}

/// MQTT source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::mqtt::MqttSourceConfig)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct MqttSourceConfigDto {
    #[serde(default = "default_mqtt_host")]
    pub host: ConfigValue<String>,

    #[serde(default = "default_mqtt_port")]
    pub port: ConfigValue<u16>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<MqttTopicConfigDto>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topic_mappings: Vec<TopicMappingDto>,

    #[serde(default = "default_event_channel_capacity_dto")]
    pub event_channel_capacity: ConfigValue<usize>,

    #[serde(
        default = "default_max_retries_dto",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_retries: Option<ConfigValue<u32>>,

    #[serde(
        default = "default_base_retry_delay_secs_dto",
        skip_serializing_if = "Option::is_none"
    )]
    pub base_retry_delay_secs: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<MqttTransportModeDto>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_channel_capacity: Option<ConfigValue<usize>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_inflight: Option<ConfigValue<u16>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_alive: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clean_start: Option<ConfigValue<bool>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_incoming_packet_size: Option<ConfigValue<usize>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_outgoing_packet_size: Option<ConfigValue<usize>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conn_timeout: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect_properties: Option<MqttConnectPropertiesDto>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe_properties: Option<MqttSubscribePropertiesDto>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_batch_size: Option<ConfigValue<usize>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_batch_size: Option<ConfigValue<usize>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_wait_ms: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_wait_ms: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_window_secs: Option<ConfigValue<u64>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_enabled: Option<ConfigValue<bool>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<ConfigValue<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<ConfigValue<String>>,
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    MqttSourceConfigDto,
    MqttTopicConfigDto,
    MqttConnectPropertiesDto,
    MqttSubscribePropertiesDto,
    MqttTransportModeDto,
    MqttTlsTransportConfigDto,
    MappingEntityDto,
    MappingModeDto,
    InjectIdDto,
    MappingPropertiesDto,
    MappingNodeDto,
    MappingRelationDto,
    TopicMappingDto,
)))]
struct MqttSourceSchemas;

impl MappingModeDto {
    fn into_runtime(self) -> MappingMode {
        match self {
            MappingModeDto::PayloadAsField => MappingMode::PayloadAsField,
            MappingModeDto::PayloadSpread => MappingMode::PayloadSpread,
        }
    }
}

impl InjectIdDto {
    fn into_runtime(self) -> InjectId {
        match self {
            InjectIdDto::True => InjectId::True,
            InjectIdDto::False => InjectId::False,
        }
    }
}

impl MqttTransportModeDto {
    fn into_runtime(self, mapper: &DtoMapper) -> anyhow::Result<MqttTransportMode> {
        match self {
            MqttTransportModeDto::Tcp => Ok(MqttTransportMode::TCP),
            MqttTransportModeDto::Tls(tls) => Ok(MqttTransportMode::TLS {
                ca: mapper.resolve_optional(&tls.ca)?,
                ca_path: mapper.resolve_optional(&tls.ca_path)?,
                alpn: tls.alpn,
                client_auth: tls.client_auth,
                client_cert_path: mapper.resolve_optional(&tls.client_cert_path)?,
                client_key_path: mapper.resolve_optional(&tls.client_key_path)?,
            }),
        }
    }
}

fn qos_from_u8(qos: u8) -> anyhow::Result<MqttQoS> {
    match qos {
        0 => Ok(MqttQoS::ZERO),
        1 => Ok(MqttQoS::ONE),
        2 => Ok(MqttQoS::TWO),
        _ => Err(anyhow!(
            "Invalid MQTT QoS value {qos}. Allowed values are 0, 1, and 2."
        )),
    }
}

/// Descriptor for the MQTT source plugin.
pub struct MqttSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for MqttSourceDescriptor {
    fn kind(&self) -> &str {
        "mqtt"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.mqtt.MqttSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = MqttSourceSchemas::openapi();
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
        let dto: MqttSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let topics = dto
            .topics
            .into_iter()
            .map(|t| {
                Ok(MqttTopicConfig {
                    topic: t.topic,
                    qos: qos_from_u8(t.qos)?,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let topic_mappings = dto
            .topic_mappings
            .into_iter()
            .map(|m| TopicMapping {
                pattern: m.pattern,
                entity: MappingEntity {
                    label: m.entity.label,
                    id: m.entity.id,
                },
                properties: MappingProperties {
                    mode: m.properties.mode.into_runtime(),
                    field_name: m.properties.field_name,
                    inject_id: m.properties.inject_id.map(InjectIdDto::into_runtime),
                    inject: m.properties.inject,
                },
                nodes: m
                    .nodes
                    .into_iter()
                    .map(|n| MappingNode {
                        label: n.label,
                        id: n.id,
                    })
                    .collect(),
                relations: m
                    .relations
                    .into_iter()
                    .map(|r| MappingRelation {
                        label: r.label,
                        from: r.from,
                        to: r.to,
                        id: r.id,
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();

        let connect_properties = match dto.connect_properties {
            Some(p) => Some(MqttConnectProperties {
                session_expiry_interval: mapper.resolve_optional(&p.session_expiry_interval)?,
                receive_maximum: mapper.resolve_optional(&p.receive_maximum)?,
                max_packet_size: mapper.resolve_optional(&p.max_packet_size)?,
                topic_alias_max: mapper.resolve_optional(&p.topic_alias_max)?,
                request_response_info: mapper.resolve_optional(&p.request_response_info)?,
                request_problem_info: mapper.resolve_optional(&p.request_problem_info)?,
                user_properties: p.user_properties,
                authentication_method: mapper.resolve_optional(&p.authentication_method)?,
                authentication_data: p.authentication_data,
            }),
            None => None,
        };

        let subscribe_properties = match dto.subscribe_properties {
            Some(p) => Some(MqttSubscribeProperties {
                id: mapper.resolve_optional(&p.id)?,
                user_properties: p.user_properties,
            }),
            None => None,
        };

        let config = MqttSourceConfig {
            host: mapper.resolve_string(&dto.host)?,
            port: mapper.resolve_typed(&dto.port)?,
            identity_provider: None,
            topics,
            topic_mappings,
            event_channel_capacity: mapper.resolve_typed(&dto.event_channel_capacity)?,
            max_retries: mapper.resolve_optional(&dto.max_retries)?,
            base_retry_delay_secs: mapper.resolve_optional(&dto.base_retry_delay_secs)?,
            transport: match dto.transport {
                Some(t) => Some(t.into_runtime(&mapper)?),
                None => None,
            },
            request_channel_capacity: mapper.resolve_optional(&dto.request_channel_capacity)?,
            max_inflight: mapper.resolve_optional(&dto.max_inflight)?,
            keep_alive: mapper.resolve_optional(&dto.keep_alive)?,
            clean_start: mapper.resolve_optional(&dto.clean_start)?,
            max_incoming_packet_size: mapper.resolve_optional(&dto.max_incoming_packet_size)?,
            max_outgoing_packet_size: mapper.resolve_optional(&dto.max_outgoing_packet_size)?,
            conn_timeout: mapper.resolve_optional(&dto.conn_timeout)?,
            connect_properties,
            subscribe_properties,
            adaptive_max_batch_size: mapper.resolve_optional(&dto.adaptive_max_batch_size)?,
            adaptive_min_batch_size: mapper.resolve_optional(&dto.adaptive_min_batch_size)?,
            adaptive_max_wait_ms: mapper.resolve_optional(&dto.adaptive_max_wait_ms)?,
            adaptive_min_wait_ms: mapper.resolve_optional(&dto.adaptive_min_wait_ms)?,
            adaptive_window_secs: mapper.resolve_optional(&dto.adaptive_window_secs)?,
            adaptive_enabled: mapper.resolve_optional(&dto.adaptive_enabled)?,
            username: mapper.resolve_optional(&dto.username)?,
            password: mapper.resolve_optional(&dto.password)?,
        };

        let source = MqttSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()
            .await?;

        Ok(Box::new(source))
    }
}
