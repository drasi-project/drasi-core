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

//! Dynamic plugin descriptor and configuration DTOs.

use std::fmt;

use drasi_plugin_sdk::prelude::*;
use drasi_source_mapping::SourceMappingDto;
use utoipa::OpenApi;

use crate::{
    config::{
        DEFAULT_ALLOW_INSECURE, DEFAULT_BUFFER_CAPACITY, DEFAULT_CONNECT_TIMEOUT_MS,
        DEFAULT_ITEMS_PATH, DEFAULT_MAX_MESSAGE_SIZE_BYTES, DEFAULT_RECONNECT_DELAY_MS,
        DEFAULT_RECONNECT_ENABLED,
    },
    mapping_config::{
        mapping_to_dto, EffectiveFromConfigSchema, ElementTemplateSchema, ElementTypeSchema,
        ExplicitEffectiveFromConfigSchema, MappingConditionSchema, OperationTypeSchema,
        SourceMappingSchema, TimestampFormatSchema,
    },
    HeaderConfig, ReconnectConfig, WebSocketSourceBuilder, WebSocketSourceConfig,
};

/// Header DTO for dynamic configuration.
#[derive(Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::HeaderConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HeaderConfigDto {
    /// Header name.
    pub name: String,
    /// Header value or secret reference.
    #[schema(value_type = ConfigValueString)]
    pub value: ConfigValue<String>,
}

impl fmt::Debug for HeaderConfigDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeaderConfigDto")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// Reconnect DTO for dynamic configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::ReconnectConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReconnectConfigDto {
    /// Whether reconnect is enabled.
    #[serde(default = "default_reconnect_enabled")]
    #[schema(value_type = ConfigValueBool)]
    pub enabled: ConfigValue<bool>,
    /// Initial exponential-backoff delay between reconnect attempts.
    #[serde(default = "default_reconnect_delay_ms")]
    #[schema(value_type = ConfigValueU64)]
    pub delay_ms: ConfigValue<u64>,
    /// Maximum exponential reconnect delay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ConfigValueU64>)]
    pub max_delay_ms: Option<ConfigValue<u64>>,
}

impl Default for ReconnectConfigDto {
    fn default() -> Self {
        Self {
            enabled: default_reconnect_enabled(),
            delay_ms: default_reconnect_delay_ms(),
            max_delay_ms: None,
        }
    }
}

/// WebSocket source configuration DTO.
#[derive(Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::WebSocketSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebSocketSourceConfigDto {
    /// WebSocket endpoint.
    #[schema(value_type = ConfigValueString)]
    pub url: ConfigValue<String>,
    /// Allows cleartext `ws://` connections.
    #[serde(default = "default_allow_insecure")]
    #[schema(value_type = ConfigValueBool)]
    pub allow_insecure: ConfigValue<bool>,
    /// Headers added to the upgrade request.
    #[serde(default)]
    pub headers: Vec<HeaderConfigDto>,
    /// Timeout for each connection attempt.
    #[serde(default = "default_connect_timeout_ms")]
    #[schema(value_type = ConfigValueU64)]
    pub connect_timeout_ms: ConfigValue<u64>,
    /// JSON messages sent after each handshake.
    #[serde(default)]
    #[schema(value_type = Vec<ConfigValueString>)]
    pub initial_messages: Vec<ConfigValue<String>>,
    /// Reconnect behavior.
    #[serde(default)]
    pub reconnect: ReconnectConfigDto,
    /// `$` for the whole frame, or one top-level array field.
    #[serde(default = "default_items_path")]
    #[schema(value_type = ConfigValueString)]
    pub items_path: ConfigValue<String>,
    /// Graph mappings.
    #[schema(value_type = Vec<source::websocket::SourceMapping>)]
    pub mappings: Vec<SourceMappingDto>,
    /// Maximum WebSocket message size.
    #[serde(default = "default_max_message_size_bytes")]
    #[schema(value_type = ConfigValueUsize)]
    pub max_message_size_bytes: ConfigValue<usize>,
    /// Capacity of each query subscriber channel.
    #[serde(default = "default_buffer_capacity")]
    #[schema(value_type = ConfigValueUsize)]
    pub buffer_capacity: ConfigValue<usize>,
}

impl fmt::Debug for WebSocketSourceConfigDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSocketSourceConfigDto")
            .field("url", &"<redacted>")
            .field("allow_insecure", &self.allow_insecure)
            .field("headers", &self.headers)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .field("initial_message_count", &self.initial_messages.len())
            .field("reconnect", &self.reconnect)
            .field("items_path", &self.items_path)
            .field("mapping_count", &self.mappings.len())
            .field("max_message_size_bytes", &self.max_message_size_bytes)
            .field("buffer_capacity", &self.buffer_capacity)
            .finish()
    }
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    WebSocketSourceConfigDto,
    HeaderConfigDto,
    ReconnectConfigDto,
    SourceMappingSchema,
    MappingConditionSchema,
    OperationTypeSchema,
    ElementTypeSchema,
    EffectiveFromConfigSchema,
    ExplicitEffectiveFromConfigSchema,
    TimestampFormatSchema,
    ElementTemplateSchema,
)))]
struct WebSocketSourceSchemas;

/// Dynamic plugin descriptor for the WebSocket source.
pub struct WebSocketSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for WebSocketSourceDescriptor {
    fn kind(&self) -> &str {
        "websocket"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.websocket.WebSocketSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        use drasi_plugin_sdk::schema_ui::SchemaUiAnnotator;

        let api = WebSocketSourceSchemas::openapi();
        let mut schemas = serde_json::to_value(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema");

        schemas["source.websocket.HeaderConfig"]["properties"]["value"]["x-ui:widget"] =
            serde_json::Value::String("password".to_string());

        SchemaUiAnnotator::new(schemas, "source.websocket.WebSocketSourceConfig")
            .expect("root schema not found")
            .field("headers", |field| field.group("Connection").order(2))
            .field("mappings", |field| field.group("Mapping").order(1))
            .annotate()
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::Source>> {
        if let Some(mappings) = config_json.get("mappings") {
            serde_json::from_value::<Vec<SourceMappingSchema>>(mappings.clone())?;
        }
        let dto: WebSocketSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let mut headers = Vec::with_capacity(dto.headers.len());
        for header in dto.headers {
            headers.push(HeaderConfig {
                name: header.name,
                value: mapper.resolve_string(&header.value).await?,
            });
        }

        let config = WebSocketSourceConfig {
            url: mapper.resolve_string(&dto.url).await?,
            allow_insecure: mapper.resolve_typed(&dto.allow_insecure).await?,
            headers,
            connect_timeout_ms: mapper.resolve_typed(&dto.connect_timeout_ms).await?,
            initial_messages: mapper.resolve_string_vec(&dto.initial_messages).await?,
            reconnect: ReconnectConfig {
                enabled: mapper.resolve_typed(&dto.reconnect.enabled).await?,
                delay_ms: mapper.resolve_typed(&dto.reconnect.delay_ms).await?,
                max_delay_ms: match dto.reconnect.max_delay_ms {
                    Some(value) => Some(mapper.resolve_typed(&value).await?),
                    None => None,
                },
            },
            items_path: mapper.resolve_string(&dto.items_path).await?,
            mappings: dto.mappings.into_iter().map(Into::into).collect(),
            max_message_size_bytes: mapper.resolve_typed(&dto.max_message_size_bytes).await?,
            buffer_capacity: mapper.resolve_typed(&dto.buffer_capacity).await?,
        };

        let mut source = WebSocketSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;
        source.base.set_raw_config(config_json.clone());

        Ok(Box::new(source))
    }
}

impl From<&WebSocketSourceConfig> for WebSocketSourceConfigDto {
    fn from(config: &WebSocketSourceConfig) -> Self {
        Self {
            url: ConfigValue::Static(config.url.clone()),
            allow_insecure: ConfigValue::Static(config.allow_insecure),
            headers: config
                .headers
                .iter()
                .map(|header| HeaderConfigDto {
                    name: header.name.clone(),
                    value: ConfigValue::Static(header.value.clone()),
                })
                .collect(),
            connect_timeout_ms: ConfigValue::Static(config.connect_timeout_ms),
            initial_messages: config
                .initial_messages
                .iter()
                .cloned()
                .map(ConfigValue::Static)
                .collect(),
            reconnect: ReconnectConfigDto {
                enabled: ConfigValue::Static(config.reconnect.enabled),
                delay_ms: ConfigValue::Static(config.reconnect.delay_ms),
                max_delay_ms: config.reconnect.max_delay_ms.map(ConfigValue::Static),
            },
            items_path: ConfigValue::Static(config.items_path.clone()),
            mappings: config.mappings.iter().map(mapping_to_dto).collect(),
            max_message_size_bytes: ConfigValue::Static(config.max_message_size_bytes),
            buffer_capacity: ConfigValue::Static(config.buffer_capacity),
        }
    }
}

fn default_allow_insecure() -> ConfigValue<bool> {
    ConfigValue::Static(DEFAULT_ALLOW_INSECURE)
}

fn default_connect_timeout_ms() -> ConfigValue<u64> {
    ConfigValue::Static(DEFAULT_CONNECT_TIMEOUT_MS)
}

fn default_reconnect_enabled() -> ConfigValue<bool> {
    ConfigValue::Static(DEFAULT_RECONNECT_ENABLED)
}

fn default_reconnect_delay_ms() -> ConfigValue<u64> {
    ConfigValue::Static(DEFAULT_RECONNECT_DELAY_MS)
}

fn default_items_path() -> ConfigValue<String> {
    ConfigValue::Static(DEFAULT_ITEMS_PATH.to_string())
}

fn default_max_message_size_bytes() -> ConfigValue<usize> {
    ConfigValue::Static(DEFAULT_MAX_MESSAGE_SIZE_BYTES)
}

fn default_buffer_capacity() -> ConfigValue<usize> {
    ConfigValue::Static(DEFAULT_BUFFER_CAPACITY)
}

#[cfg(test)]
mod tests {
    use drasi_lib::Source;

    use super::*;

    fn config_json() -> serde_json::Value {
        serde_json::json!({
            "url": "wss://example.com/events",
            "mappings": [{
                "operation": "insert",
                "elementType": "node",
                "effectiveFrom": {
                    "value": "{{payload.timestamp}}",
                    "format": "unix_millis"
                },
                "template": {
                    "id": "{{payload.id}}",
                    "labels": ["Sensor"],
                    "properties": {
                        "value": "{{payload.value}}"
                    }
                }
            }]
        })
    }

    fn mapping_fixtures() -> Vec<serde_json::Value> {
        vec![
            serde_json::json!({
                "when": {"field": "payload.kind", "equals": "sensor"},
                "operation": "insert",
                "elementType": "node",
                "effectiveFrom": "{{payload.observedAt}}",
                "template": {
                    "id": "{{payload.id}}",
                    "labels": ["Sensor"],
                    "properties": {"value": "{{payload.value}}"}
                }
            }),
            serde_json::json!({
                "when": {"field": "payload.kind", "contains": "reading"},
                "operation": "update",
                "elementType": "relation",
                "effectiveFrom": {
                    "value": "{{payload.observedAt}}",
                    "format": "iso8601"
                },
                "template": {
                    "id": "{{payload.id}}",
                    "labels": ["READS"],
                    "properties": {"value": "{{payload.value}}"},
                    "from": "{{payload.readerId}}",
                    "to": "{{payload.sensorId}}"
                }
            }),
            serde_json::json!({
                "when": {"field": "payload.kind", "regex": "^deleted$"},
                "operation": "delete",
                "elementType": "node",
                "effectiveFrom": {
                    "value": "{{payload.observedAt}}",
                    "format": "unix_seconds"
                },
                "template": {
                    "id": "{{payload.id}}",
                    "labels": ["Sensor"]
                }
            }),
            serde_json::json!({
                "operationFrom": "payload.operation",
                "operationMap": {
                    "created": "insert",
                    "changed": "update",
                    "removed": "delete"
                },
                "elementType": "node",
                "effectiveFrom": {
                    "value": "{{payload.observedAt}}",
                    "format": "unix_millis"
                },
                "template": {
                    "id": "{{payload.id}}",
                    "labels": ["Sensor"]
                }
            }),
            serde_json::json!({
                "operation": "insert",
                "elementType": "node",
                "effectiveFrom": {
                    "value": "{{payload.observedAt}}",
                    "format": "unix_nanos"
                },
                "template": {
                    "id": "{{payload.id}}",
                    "labels": ["Sensor"]
                }
            }),
        ]
    }

    mod schema {
        use super::*;

        #[test]
        fn exposes_expected_descriptor_metadata() {
            let descriptor = WebSocketSourceDescriptor;
            assert_eq!(descriptor.kind(), "websocket");
            assert_eq!(descriptor.config_version(), "1.0.0");
            assert_eq!(
                descriptor.config_schema_name(),
                "source.websocket.WebSocketSourceConfig"
            );
            assert!(descriptor
                .config_schema_json()
                .contains("source.websocket.WebSocketSourceConfig"));
        }

        #[test]
        fn config_schema_references_strict_mapping_schemas() {
            let descriptor = WebSocketSourceDescriptor;
            let schemas: serde_json::Value =
                serde_json::from_str(&descriptor.config_schema_json()).unwrap();

            assert!(schemas.get("source.websocket.SourceMapping").is_some());
            assert_eq!(
                schemas["source.websocket.WebSocketSourceConfig"]["properties"]["mappings"]
                    ["items"]["$ref"],
                "#/components/schemas/source.websocket.SourceMapping"
            );
            for schema in [
                "source.websocket.SourceMapping",
                "source.websocket.MappingCondition",
                "source.websocket.ExplicitEffectiveFromConfig",
                "source.websocket.ElementTemplate",
            ] {
                assert_eq!(schemas[schema]["additionalProperties"], false);
            }
        }

        #[test]
        fn config_schema_marks_header_values_as_secret() {
            let schemas: serde_json::Value =
                serde_json::from_str(&WebSocketSourceDescriptor.config_schema_json()).unwrap();

            assert_eq!(
                schemas["source.websocket.HeaderConfig"]["properties"]["value"]["x-ui:widget"],
                "password"
            );
        }

        #[test]
        fn config_schema_omits_unsupported_match_policy() {
            let schemas: serde_json::Value =
                serde_json::from_str(&WebSocketSourceDescriptor.config_schema_json()).unwrap();

            assert!(
                schemas["source.websocket.WebSocketSourceConfig"]["properties"]
                    .get("matchPolicy")
                    .is_none()
            );
        }

        #[test]
        fn config_schema_exposes_maximum_reconnect_delay() {
            let schemas: serde_json::Value =
                serde_json::from_str(&WebSocketSourceDescriptor.config_schema_json()).unwrap();

            assert!(schemas["source.websocket.ReconnectConfig"]["properties"]
                .get("maxDelayMs")
                .is_some());
        }
    }

    mod defaults {
        use super::*;

        #[test]
        fn descriptor_defaults_match_runtime_defaults() {
            let dto: WebSocketSourceConfigDto = serde_json::from_value(serde_json::json!({
                "url": "wss://example.com/events",
                "mappings": [{
                    "operation": "insert",
                    "elementType": "node",
                    "template": {
                        "id": "{{payload.id}}",
                        "labels": ["Sensor"]
                    }
                }]
            }))
            .unwrap();
            let runtime = WebSocketSourceConfig::default();

            assert_eq!(
                dto.allow_insecure,
                ConfigValue::Static(runtime.allow_insecure)
            );
            assert_eq!(
                dto.connect_timeout_ms,
                ConfigValue::Static(runtime.connect_timeout_ms)
            );
            assert_eq!(
                dto.reconnect.enabled,
                ConfigValue::Static(runtime.reconnect.enabled)
            );
            assert_eq!(
                dto.reconnect.delay_ms,
                ConfigValue::Static(runtime.reconnect.delay_ms)
            );
            assert_eq!(dto.reconnect.max_delay_ms, None);
            assert_eq!(dto.items_path, ConfigValue::Static(runtime.items_path));
            assert_eq!(
                dto.max_message_size_bytes,
                ConfigValue::Static(runtime.max_message_size_bytes)
            );
            assert_eq!(
                dto.buffer_capacity,
                ConfigValue::Static(runtime.buffer_capacity)
            );
        }
    }

    mod creation {
        use super::*;

        #[tokio::test]
        async fn creates_source_with_requested_auto_start_and_derived_schema() {
            let source = WebSocketSourceDescriptor
                .create_source("source", &config_json(), false)
                .await
                .unwrap();

            assert_eq!(source.type_name(), "websocket");
            assert!(!source.auto_start());
            assert_eq!(source.describe_schema().unwrap().nodes[0].label, "Sensor");
        }
    }

    mod raw_properties {
        use super::*;

        #[tokio::test]
        async fn preserves_descriptor_source_raw_properties() {
            let config = config_json();
            let source = WebSocketSourceDescriptor
                .create_source("source", &config, false)
                .await
                .unwrap();

            assert_eq!(serde_json::to_value(source.properties()).unwrap(), config);
        }
    }

    mod schema_validation {
        use drasi_source_mapping::{
            EffectiveFromConfigDto, ElementTemplateDto, ElementTypeDto, MappingConditionDto,
            OperationTypeDto, TimestampFormatDto,
        };

        use super::*;

        impl From<SourceMappingDto> for SourceMappingSchema {
            fn from(dto: SourceMappingDto) -> Self {
                let SourceMappingDto {
                    when,
                    operation,
                    operation_from,
                    operation_map,
                    element_type,
                    effective_from,
                    template,
                } = dto;
                Self {
                    when: when.map(Into::into),
                    operation: operation.map(Into::into),
                    operation_from,
                    operation_map: operation_map.map(|operations| {
                        operations
                            .into_iter()
                            .map(|(name, operation)| (name, operation.into()))
                            .collect()
                    }),
                    element_type: element_type.into(),
                    effective_from: effective_from.map(Into::into),
                    template: template.into(),
                }
            }
        }

        impl From<MappingConditionDto> for MappingConditionSchema {
            fn from(dto: MappingConditionDto) -> Self {
                let MappingConditionDto {
                    header,
                    field,
                    equals,
                    contains,
                    regex,
                } = dto;
                let _ = header;
                Self {
                    field,
                    equals,
                    contains,
                    regex,
                }
            }
        }

        impl From<OperationTypeDto> for OperationTypeSchema {
            fn from(dto: OperationTypeDto) -> Self {
                match dto {
                    OperationTypeDto::Insert => Self::Insert,
                    OperationTypeDto::Update => Self::Update,
                    OperationTypeDto::Delete => Self::Delete,
                }
            }
        }

        impl From<ElementTypeDto> for ElementTypeSchema {
            fn from(dto: ElementTypeDto) -> Self {
                match dto {
                    ElementTypeDto::Node => Self::Node,
                    ElementTypeDto::Relation => Self::Relation,
                }
            }
        }

        impl From<EffectiveFromConfigDto> for EffectiveFromConfigSchema {
            fn from(dto: EffectiveFromConfigDto) -> Self {
                match dto {
                    EffectiveFromConfigDto::Simple(value) => Self::Simple(value),
                    EffectiveFromConfigDto::Explicit { value, format } => {
                        Self::Explicit(ExplicitEffectiveFromConfigSchema {
                            value,
                            format: format.into(),
                        })
                    }
                }
            }
        }

        impl From<TimestampFormatDto> for TimestampFormatSchema {
            fn from(dto: TimestampFormatDto) -> Self {
                match dto {
                    TimestampFormatDto::Iso8601 => Self::Iso8601,
                    TimestampFormatDto::UnixSeconds => Self::UnixSeconds,
                    TimestampFormatDto::UnixMillis => Self::UnixMillis,
                    TimestampFormatDto::UnixNanos => Self::UnixNanos,
                }
            }
        }

        impl From<ElementTemplateDto> for ElementTemplateSchema {
            fn from(dto: ElementTemplateDto) -> Self {
                let ElementTemplateDto {
                    id,
                    labels,
                    properties,
                    from,
                    to,
                } = dto;
                Self {
                    id,
                    labels,
                    properties,
                    from,
                    to,
                }
            }
        }

        #[test]
        fn shared_dto_and_websocket_schema_have_serde_parity() {
            for fixture in mapping_fixtures() {
                let shared: SourceMappingDto =
                    serde_json::from_value(fixture.clone()).expect("shared DTO must parse fixture");
                let local: SourceMappingSchema =
                    serde_json::from_value(fixture).expect("WebSocket schema must parse fixture");
                let adapted: SourceMappingSchema = shared.clone().into();
                assert_eq!(local, adapted);

                let shared_json =
                    serde_json::to_value(&shared).expect("shared DTO must serialize directly");
                let reparsed: SourceMappingSchema = serde_json::from_value(shared_json.clone())
                    .expect("WebSocket schema must parse serialized shared DTO");
                assert_eq!(serde_json::to_value(&local).unwrap(), shared_json);
                assert_eq!(reparsed, local);
            }
        }

        #[tokio::test]
        async fn rejects_unknown_nested_mapping_fields() {
            let mut unknown_mapping = config_json();
            unknown_mapping["mappings"][0]["unexpectedMapping"] = serde_json::json!(true);
            let mut misspelled_condition = config_json();
            misspelled_condition["mappings"][0]["when"] = serde_json::json!({
                "field": "envelope.type",
                "equal": "batch"
            });
            let mut misspelled_template = config_json();
            misspelled_template["mappings"][0]["template"]["propertiez"] =
                serde_json::json!({"value": "{{payload.value}}"});
            let mut misspelled_regex = config_json();
            misspelled_regex["mappings"][0]["when"] = serde_json::json!({
                "field": "envelope.type",
                "regexp": "^batch$"
            });

            for (unknown_field, config) in [
                ("unexpectedMapping", unknown_mapping),
                ("equal", misspelled_condition),
                ("propertiez", misspelled_template),
                ("regexp", misspelled_regex),
            ] {
                let error = WebSocketSourceDescriptor
                    .create_source("source", &config, false)
                    .await
                    .err()
                    .expect("unknown nested field should be rejected");
                let message = error.to_string();
                assert!(message.contains("unknown field"), "{message}");
                assert!(message.contains(unknown_field), "{message}");
            }
        }

        #[tokio::test]
        async fn rejects_unknown_explicit_effective_from_fields() {
            let mut config = config_json();
            config["mappings"][0]["effectiveFrom"]["unexpected"] = serde_json::json!("unix_millis");

            let error = WebSocketSourceDescriptor
                .create_source("source", &config, false)
                .await
                .err()
                .expect("unknown explicit effectiveFrom field should be rejected");
            let message = error.to_string();
            assert!(message.contains("unknown field"), "{message}");
            assert!(message.contains("unexpected"), "{message}");
        }
    }

    mod redaction {
        use super::*;

        #[tokio::test]
        async fn preserves_raw_environment_variable_header_value() {
            let mut config = config_json();
            config["headers"] = serde_json::json!([{
                "name": "Authorization",
                "value": {
                    "kind": "EnvironmentVariable",
                    "name": "DRASI_WEBSOCKET_TEST_AUTHORIZATION",
                    "default": "Bearer resolved-value"
                }
            }]);

            let source = WebSocketSourceDescriptor
                .create_source("source", &config, true)
                .await
                .unwrap();

            assert_eq!(source.properties().get("headers"), config.get("headers"));
        }

        #[test]
        fn dto_debug_redacts_static_sensitive_values() {
            let dto: WebSocketSourceConfigDto = serde_json::from_value(serde_json::json!({
                "url": "wss://example.com/events?token=url-secret",
                "headers": [{
                    "name": "Authorization",
                    "value": "header-secret"
                }],
                "initialMessages": ["{\"token\":\"message-secret\"}"],
                "mappings": [{
                    "operation": "insert",
                    "elementType": "node",
                    "template": {
                        "id": "{{payload.id}}",
                        "labels": ["Sensor"]
                    }
                }]
            }))
            .unwrap();

            let debug = format!("{dto:?}");
            assert!(!debug.contains("url-secret"));
            assert!(!debug.contains("header-secret"));
            assert!(!debug.contains("message-secret"));
            assert!(debug.contains("<redacted>"));
        }
    }
}
