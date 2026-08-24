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

use std::{collections::HashMap, fmt};

use drasi_plugin_sdk::prelude::*;
use drasi_source_mapping::{
    EffectiveFromConfig, EffectiveFromConfigDto, ElementTemplate, ElementTemplateDto, ElementType,
    ElementTypeDto, MappingCondition, MappingConditionDto, OperationType, OperationTypeDto,
    SourceMapping, SourceMappingDto, TimestampFormat, TimestampFormatDto,
};
use utoipa::OpenApi;

use crate::{HeaderConfig, ReconnectConfig, WebSocketSourceBuilder, WebSocketSourceConfig};

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

/// OpenAPI shape for a WebSocket source mapping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::SourceMapping)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourceMappingSchema {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<MappingConditionSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<OperationTypeSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_map: Option<HashMap<String, OperationTypeSchema>>,
    pub element_type: ElementTypeSchema,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_from: Option<EffectiveFromConfigSchema>,
    pub template: ElementTemplateSchema,
}

/// OpenAPI shape for a WebSocket mapping condition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::MappingCondition)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MappingConditionSchema {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

/// OpenAPI shape for a mapping operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::OperationType)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OperationTypeSchema {
    Insert,
    Update,
    Delete,
}

/// OpenAPI shape for a mapped graph element.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::ElementType)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ElementTypeSchema {
    Node,
    Relation,
}

/// OpenAPI shape for a mapping timestamp.
#[derive(Debug, Clone, Serialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::EffectiveFromConfig)]
#[serde(untagged)]
pub(crate) enum EffectiveFromConfigSchema {
    Simple(String),
    Explicit(ExplicitEffectiveFromConfigSchema),
}

impl<'de> Deserialize<'de> for EffectiveFromConfigSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(value) => Ok(Self::Simple(value)),
            serde_json::Value::Object(_) => serde_json::from_value(value)
                .map(Self::Explicit)
                .map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(
                "effectiveFrom must be a template string or explicit object",
            )),
        }
    }
}

/// OpenAPI shape for an explicit mapping timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::ExplicitEffectiveFromConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExplicitEffectiveFromConfigSchema {
    pub value: String,
    pub format: TimestampFormatSchema,
}

/// OpenAPI shape for an explicit timestamp format.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::TimestampFormat)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TimestampFormatSchema {
    Iso8601,
    UnixSeconds,
    UnixMillis,
    UnixNanos,
}

/// OpenAPI shape for an element template.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = source::websocket::ElementTemplate)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ElementTemplateSchema {
    pub id: String,
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
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

fn mapping_to_dto(mapping: &SourceMapping) -> SourceMappingDto {
    SourceMappingDto {
        when: mapping.when.as_ref().map(condition_to_dto),
        operation: mapping.operation.as_ref().map(operation_to_dto),
        operation_from: mapping.operation_from.clone(),
        operation_map: mapping.operation_map.as_ref().map(|operations| {
            operations
                .iter()
                .map(|(name, operation)| (name.clone(), operation_to_dto(operation)))
                .collect::<HashMap<_, _>>()
        }),
        element_type: element_type_to_dto(&mapping.element_type),
        effective_from: mapping.effective_from.as_ref().map(effective_from_to_dto),
        template: template_to_dto(&mapping.template),
    }
}

fn condition_to_dto(condition: &MappingCondition) -> MappingConditionDto {
    MappingConditionDto {
        header: condition.header.clone(),
        field: condition.field.clone(),
        equals: condition.equals.clone(),
        contains: condition.contains.clone(),
        regex: condition.regex.clone(),
    }
}

fn operation_to_dto(operation: &OperationType) -> OperationTypeDto {
    match operation {
        OperationType::Insert => OperationTypeDto::Insert,
        OperationType::Update => OperationTypeDto::Update,
        OperationType::Delete => OperationTypeDto::Delete,
    }
}

fn element_type_to_dto(element_type: &ElementType) -> ElementTypeDto {
    match element_type {
        ElementType::Node => ElementTypeDto::Node,
        ElementType::Relation => ElementTypeDto::Relation,
    }
}

fn effective_from_to_dto(effective_from: &EffectiveFromConfig) -> EffectiveFromConfigDto {
    match effective_from {
        EffectiveFromConfig::Simple(value) => EffectiveFromConfigDto::Simple(value.clone()),
        EffectiveFromConfig::Explicit { value, format } => EffectiveFromConfigDto::Explicit {
            value: value.clone(),
            format: timestamp_format_to_dto(format),
        },
    }
}

fn timestamp_format_to_dto(format: &TimestampFormat) -> TimestampFormatDto {
    match format {
        TimestampFormat::Iso8601 => TimestampFormatDto::Iso8601,
        TimestampFormat::UnixSeconds => TimestampFormatDto::UnixSeconds,
        TimestampFormat::UnixMillis => TimestampFormatDto::UnixMillis,
        TimestampFormat::UnixNanos => TimestampFormatDto::UnixNanos,
    }
}

fn template_to_dto(template: &ElementTemplate) -> ElementTemplateDto {
    ElementTemplateDto {
        id: template.id.clone(),
        labels: template.labels.clone(),
        properties: template.properties.clone(),
        from: template.from.clone(),
        to: template.to.clone(),
    }
}

fn default_allow_insecure() -> ConfigValue<bool> {
    ConfigValue::Static(false)
}

fn default_connect_timeout_ms() -> ConfigValue<u64> {
    ConfigValue::Static(10_000)
}

fn default_reconnect_enabled() -> ConfigValue<bool> {
    ConfigValue::Static(true)
}

fn default_reconnect_delay_ms() -> ConfigValue<u64> {
    ConfigValue::Static(1_000)
}

fn default_items_path() -> ConfigValue<String> {
    ConfigValue::Static("$".to_string())
}

fn default_max_message_size_bytes() -> ConfigValue<usize> {
    ConfigValue::Static(1024 * 1024)
}

fn default_buffer_capacity() -> ConfigValue<usize> {
    ConfigValue::Static(64)
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
            schemas["source.websocket.WebSocketSourceConfig"]["properties"]["mappings"]["items"]
                ["$ref"],
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

    #[tokio::test]
    async fn preserves_descriptor_source_raw_properties() {
        let config = config_json();
        let source = WebSocketSourceDescriptor
            .create_source("source", &config, false)
            .await
            .unwrap();

        assert_eq!(serde_json::to_value(source.properties()).unwrap(), config);
    }

    #[tokio::test]
    async fn rejects_unknown_nested_mapping_fields() {
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
