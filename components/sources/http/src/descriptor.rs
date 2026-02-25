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

//! HTTP source plugin descriptor and configuration DTOs.

use crate::config::{
    AuthConfig, BearerConfig, CorsConfig, EffectiveFromConfig, ElementTemplate, ElementType,
    ErrorBehavior, HttpMethod, MappingCondition, OperationType, SignatureAlgorithm,
    SignatureConfig, SignatureEncoding, TimestampFormat, WebhookConfig, WebhookMapping,
    WebhookRoute,
};
use crate::{HttpSourceBuilder, HttpSourceConfig};
use drasi_plugin_sdk::prelude::*;
use std::collections::HashMap;
use utoipa::OpenApi;

/// HTTP source configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = HttpSourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpSourceConfigDto {
    pub host: ConfigValue<String>,
    pub port: ConfigValue<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<ConfigValue<String>>,
    #[serde(default = "default_http_timeout_ms")]
    pub timeout_ms: ConfigValue<u64>,
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
    #[schema(value_type = Option<WebhookConfig>)]
    pub webhooks: Option<WebhookConfigDto>,
}

fn default_http_timeout_ms() -> ConfigValue<u64> {
    ConfigValue::Static(10000)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = WebhookConfig)]
#[serde(rename_all = "camelCase")]
pub struct WebhookConfigDto {
    #[serde(default)]
    #[schema(value_type = ErrorBehavior)]
    pub error_behavior: ErrorBehaviorDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<CorsConfig>)]
    pub cors: Option<CorsConfigDto>,
    #[schema(value_type = Vec<WebhookRoute>)]
    pub routes: Vec<WebhookRouteDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = CorsConfig)]
#[serde(rename_all = "camelCase")]
pub struct CorsConfigDto {
    #[serde(default = "default_cors_enabled")]
    pub enabled: bool,
    #[serde(default = "default_cors_origins")]
    pub allow_origins: Vec<ConfigValue<String>>,
    #[serde(default = "default_cors_methods")]
    pub allow_methods: Vec<ConfigValue<String>>,
    #[serde(default = "default_cors_headers")]
    pub allow_headers: Vec<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<ConfigValue<String>>,
    #[serde(default)]
    pub allow_credentials: bool,
    #[serde(default = "default_cors_max_age")]
    pub max_age: u64,
}

fn default_cors_enabled() -> bool {
    true
}

fn default_cors_origins() -> Vec<ConfigValue<String>> {
    vec![ConfigValue::Static("*".to_string())]
}

fn default_cors_methods() -> Vec<ConfigValue<String>> {
    vec![
        ConfigValue::Static("GET".to_string()),
        ConfigValue::Static("POST".to_string()),
        ConfigValue::Static("PUT".to_string()),
        ConfigValue::Static("PATCH".to_string()),
        ConfigValue::Static("DELETE".to_string()),
        ConfigValue::Static("OPTIONS".to_string()),
    ]
}

fn default_cors_headers() -> Vec<ConfigValue<String>> {
    vec![
        ConfigValue::Static("Content-Type".to_string()),
        ConfigValue::Static("Authorization".to_string()),
        ConfigValue::Static("X-Requested-With".to_string()),
    ]
}

fn default_cors_max_age() -> u64 {
    3600
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default, utoipa::ToSchema)]
#[schema(as = ErrorBehavior)]
#[serde(rename_all = "snake_case")]
pub enum ErrorBehaviorDto {
    #[default]
    AcceptAndLog,
    AcceptAndSkip,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = WebhookRoute)]
#[serde(rename_all = "camelCase")]
pub struct WebhookRouteDto {
    pub path: ConfigValue<String>,
    #[serde(default = "default_methods")]
    #[schema(value_type = Vec<HttpMethod>)]
    pub methods: Vec<HttpMethodDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<AuthConfig>)]
    pub auth: Option<AuthConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<ErrorBehavior>)]
    pub error_behavior: Option<ErrorBehaviorDto>,
    #[schema(value_type = Vec<WebhookMapping>)]
    pub mappings: Vec<WebhookMappingDto>,
}

fn default_methods() -> Vec<HttpMethodDto> {
    vec![HttpMethodDto::Post]
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash, utoipa::ToSchema)]
#[schema(as = HttpMethod)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethodDto {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = AuthConfig)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfigDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<SignatureConfig>)]
    pub signature: Option<SignatureConfigDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<BearerConfig>)]
    pub bearer: Option<BearerConfigDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = SignatureConfig)]
#[serde(rename_all = "camelCase")]
pub struct SignatureConfigDto {
    #[serde(rename = "type")]
    #[schema(value_type = SignatureAlgorithm)]
    pub algorithm: SignatureAlgorithmDto,
    pub secret_env: ConfigValue<String>,
    pub header: ConfigValue<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<ConfigValue<String>>,
    #[serde(default)]
    #[schema(value_type = SignatureEncoding)]
    pub encoding: SignatureEncodingDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = SignatureAlgorithm)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureAlgorithmDto {
    HmacSha1,
    HmacSha256,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default, utoipa::ToSchema)]
#[schema(as = SignatureEncoding)]
#[serde(rename_all = "lowercase")]
pub enum SignatureEncodingDto {
    #[default]
    Hex,
    Base64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = BearerConfig)]
#[serde(rename_all = "camelCase")]
pub struct BearerConfigDto {
    pub token_env: ConfigValue<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = WebhookMapping)]
#[serde(rename_all = "camelCase")]
pub struct WebhookMappingDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<MappingCondition>)]
    pub when: Option<MappingConditionDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<OperationType>)]
    pub operation: Option<OperationTypeDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_from: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<HashMap<String, OperationType>>)]
    pub operation_map: Option<HashMap<String, OperationTypeDto>>,
    #[schema(value_type = ElementType)]
    pub element_type: ElementTypeDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<EffectiveFromConfig>)]
    pub effective_from: Option<EffectiveFromConfigDto>,
    #[schema(value_type = ElementTemplate)]
    pub template: ElementTemplateDto,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = MappingCondition)]
#[serde(rename_all = "camelCase")]
pub struct MappingConditionDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<ConfigValue<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = OperationType)]
#[serde(rename_all = "lowercase")]
pub enum OperationTypeDto {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = ElementType)]
#[serde(rename_all = "lowercase")]
pub enum ElementTypeDto {
    Node,
    Relation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = EffectiveFromConfig)]
#[serde(untagged)]
pub enum EffectiveFromConfigDto {
    Simple(ConfigValue<String>),
    Explicit {
        value: ConfigValue<String>,
        format: TimestampFormatDto,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = TimestampFormat)]
#[serde(rename_all = "snake_case")]
pub enum TimestampFormatDto {
    Iso8601,
    UnixSeconds,
    UnixMillis,
    UnixNanos,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = ElementTemplate)]
#[serde(rename_all = "camelCase")]
pub struct ElementTemplateDto {
    pub id: ConfigValue<String>,
    pub labels: Vec<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ConfigValue<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<ConfigValue<String>>,
}

// --- Mapping functions ---

fn map_error_behavior(dto: &ErrorBehaviorDto) -> ErrorBehavior {
    match dto {
        ErrorBehaviorDto::AcceptAndLog => ErrorBehavior::AcceptAndLog,
        ErrorBehaviorDto::AcceptAndSkip => ErrorBehavior::AcceptAndSkip,
        ErrorBehaviorDto::Reject => ErrorBehavior::Reject,
    }
}

fn map_http_method(dto: &HttpMethodDto) -> HttpMethod {
    match dto {
        HttpMethodDto::Get => HttpMethod::Get,
        HttpMethodDto::Post => HttpMethod::Post,
        HttpMethodDto::Put => HttpMethod::Put,
        HttpMethodDto::Patch => HttpMethod::Patch,
        HttpMethodDto::Delete => HttpMethod::Delete,
    }
}

fn map_signature_algorithm(dto: &SignatureAlgorithmDto) -> SignatureAlgorithm {
    match dto {
        SignatureAlgorithmDto::HmacSha1 => SignatureAlgorithm::HmacSha1,
        SignatureAlgorithmDto::HmacSha256 => SignatureAlgorithm::HmacSha256,
    }
}

fn map_signature_encoding(dto: &SignatureEncodingDto) -> SignatureEncoding {
    match dto {
        SignatureEncodingDto::Hex => SignatureEncoding::Hex,
        SignatureEncodingDto::Base64 => SignatureEncoding::Base64,
    }
}

fn map_operation_type(dto: &OperationTypeDto) -> OperationType {
    match dto {
        OperationTypeDto::Insert => OperationType::Insert,
        OperationTypeDto::Update => OperationType::Update,
        OperationTypeDto::Delete => OperationType::Delete,
    }
}

fn map_element_type(dto: &ElementTypeDto) -> ElementType {
    match dto {
        ElementTypeDto::Node => ElementType::Node,
        ElementTypeDto::Relation => ElementType::Relation,
    }
}

fn map_timestamp_format(dto: &TimestampFormatDto) -> TimestampFormat {
    match dto {
        TimestampFormatDto::Iso8601 => TimestampFormat::Iso8601,
        TimestampFormatDto::UnixSeconds => TimestampFormat::UnixSeconds,
        TimestampFormatDto::UnixMillis => TimestampFormat::UnixMillis,
        TimestampFormatDto::UnixNanos => TimestampFormat::UnixNanos,
    }
}

fn map_webhook_config(
    dto: &WebhookConfigDto,
    resolver: &DtoMapper,
) -> Result<WebhookConfig, MappingError> {
    Ok(WebhookConfig {
        error_behavior: map_error_behavior(&dto.error_behavior),
        cors: dto
            .cors
            .as_ref()
            .map(|c| map_cors_config(c, resolver))
            .transpose()?,
        routes: dto
            .routes
            .iter()
            .map(|r| map_webhook_route(r, resolver))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn map_cors_config(dto: &CorsConfigDto, resolver: &DtoMapper) -> Result<CorsConfig, MappingError> {
    Ok(CorsConfig {
        enabled: dto.enabled,
        allow_origins: resolver.resolve_string_vec(&dto.allow_origins)?,
        allow_methods: resolver.resolve_string_vec(&dto.allow_methods)?,
        allow_headers: resolver.resolve_string_vec(&dto.allow_headers)?,
        expose_headers: resolver.resolve_string_vec(&dto.expose_headers)?,
        allow_credentials: dto.allow_credentials,
        max_age: dto.max_age,
    })
}

fn map_webhook_route(
    dto: &WebhookRouteDto,
    resolver: &DtoMapper,
) -> Result<WebhookRoute, MappingError> {
    Ok(WebhookRoute {
        path: resolver.resolve_string(&dto.path)?,
        methods: dto.methods.iter().map(map_http_method).collect(),
        auth: dto
            .auth
            .as_ref()
            .map(|a| map_auth_config(a, resolver))
            .transpose()?,
        error_behavior: dto.error_behavior.as_ref().map(map_error_behavior),
        mappings: dto
            .mappings
            .iter()
            .map(|m| map_webhook_mapping(m, resolver))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn map_auth_config(dto: &AuthConfigDto, resolver: &DtoMapper) -> Result<AuthConfig, MappingError> {
    Ok(AuthConfig {
        signature: dto
            .signature
            .as_ref()
            .map(|s| map_signature_config(s, resolver))
            .transpose()?,
        bearer: dto
            .bearer
            .as_ref()
            .map(|b| map_bearer_config(b, resolver))
            .transpose()?,
    })
}

fn map_signature_config(
    dto: &SignatureConfigDto,
    resolver: &DtoMapper,
) -> Result<SignatureConfig, MappingError> {
    Ok(SignatureConfig {
        algorithm: map_signature_algorithm(&dto.algorithm),
        secret_env: resolver.resolve_string(&dto.secret_env)?,
        header: resolver.resolve_string(&dto.header)?,
        prefix: resolver.resolve_optional_string(&dto.prefix)?,
        encoding: map_signature_encoding(&dto.encoding),
    })
}

fn map_bearer_config(
    dto: &BearerConfigDto,
    resolver: &DtoMapper,
) -> Result<BearerConfig, MappingError> {
    Ok(BearerConfig {
        token_env: resolver.resolve_string(&dto.token_env)?,
    })
}

fn map_webhook_mapping(
    dto: &WebhookMappingDto,
    resolver: &DtoMapper,
) -> Result<WebhookMapping, MappingError> {
    Ok(WebhookMapping {
        when: dto
            .when
            .as_ref()
            .map(|c| map_mapping_condition(c, resolver))
            .transpose()?,
        operation: dto.operation.as_ref().map(map_operation_type),
        operation_from: resolver.resolve_optional_string(&dto.operation_from)?,
        operation_map: dto.operation_map.as_ref().map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), map_operation_type(v)))
                .collect()
        }),
        element_type: map_element_type(&dto.element_type),
        effective_from: dto
            .effective_from
            .as_ref()
            .map(|e| map_effective_from(e, resolver))
            .transpose()?,
        template: map_element_template(&dto.template, resolver)?,
    })
}

fn map_mapping_condition(
    dto: &MappingConditionDto,
    resolver: &DtoMapper,
) -> Result<MappingCondition, MappingError> {
    Ok(MappingCondition {
        header: resolver.resolve_optional_string(&dto.header)?,
        field: resolver.resolve_optional_string(&dto.field)?,
        equals: resolver.resolve_optional_string(&dto.equals)?,
        contains: resolver.resolve_optional_string(&dto.contains)?,
        regex: resolver.resolve_optional_string(&dto.regex)?,
    })
}

fn map_effective_from(
    dto: &EffectiveFromConfigDto,
    resolver: &DtoMapper,
) -> Result<EffectiveFromConfig, MappingError> {
    match dto {
        EffectiveFromConfigDto::Simple(v) => {
            Ok(EffectiveFromConfig::Simple(resolver.resolve_string(v)?))
        }
        EffectiveFromConfigDto::Explicit { value, format } => Ok(EffectiveFromConfig::Explicit {
            value: resolver.resolve_string(value)?,
            format: map_timestamp_format(format),
        }),
    }
}

fn map_element_template(
    dto: &ElementTemplateDto,
    resolver: &DtoMapper,
) -> Result<ElementTemplate, MappingError> {
    Ok(ElementTemplate {
        id: resolver.resolve_string(&dto.id)?,
        labels: resolver.resolve_string_vec(&dto.labels)?,
        properties: dto.properties.clone(),
        from: resolver.resolve_optional_string(&dto.from)?,
        to: resolver.resolve_optional_string(&dto.to)?,
    })
}

#[derive(OpenApi)]
#[openapi(components(schemas(
    HttpSourceConfigDto,
    WebhookConfigDto,
    CorsConfigDto,
    ErrorBehaviorDto,
    WebhookRouteDto,
    HttpMethodDto,
    AuthConfigDto,
    SignatureConfigDto,
    SignatureAlgorithmDto,
    SignatureEncodingDto,
    BearerConfigDto,
    WebhookMappingDto,
    MappingConditionDto,
    OperationTypeDto,
    ElementTypeDto,
    EffectiveFromConfigDto,
    TimestampFormatDto,
    ElementTemplateDto,
)))]
struct HttpSourceSchemas;

/// Descriptor for the HTTP source plugin.
pub struct HttpSourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for HttpSourceDescriptor {
    fn kind(&self) -> &str {
        "http"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "HttpSourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = HttpSourceSchemas::openapi();
        serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap()
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: HttpSourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let config = HttpSourceConfig {
            host: mapper.resolve_string(&dto.host)?,
            port: mapper.resolve_typed(&dto.port)?,
            endpoint: mapper.resolve_optional(&dto.endpoint)?,
            timeout_ms: mapper.resolve_typed(&dto.timeout_ms)?,
            adaptive_max_batch_size: mapper.resolve_optional(&dto.adaptive_max_batch_size)?,
            adaptive_min_batch_size: mapper.resolve_optional(&dto.adaptive_min_batch_size)?,
            adaptive_max_wait_ms: mapper.resolve_optional(&dto.adaptive_max_wait_ms)?,
            adaptive_min_wait_ms: mapper.resolve_optional(&dto.adaptive_min_wait_ms)?,
            adaptive_window_secs: mapper.resolve_optional(&dto.adaptive_window_secs)?,
            adaptive_enabled: mapper.resolve_optional(&dto.adaptive_enabled)?,
            webhooks: dto
                .webhooks
                .as_ref()
                .map(|w| map_webhook_config(w, &mapper))
                .transpose()?,
        };

        let source = HttpSourceBuilder::new(id)
            .with_config(config)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
