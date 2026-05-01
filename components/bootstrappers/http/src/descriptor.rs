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

//! Plugin descriptor for the HTTP bootstrap provider.

use std::collections::HashMap;

use drasi_lib::bootstrap::BootstrapProvider;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

use crate::config::{
    ApiKeyLocation, AuthConfig, ContentTypeOverride, ElementMappingConfig, ElementTemplate,
    ElementType, EndpointConfig, HttpBootstrapConfig, HttpMethod, PaginationConfig, ResponseConfig,
};
use crate::provider::HttpBootstrapProvider;

// ── DTO types ────────────────────────────────────────────────────────────────

/// Top-level configuration DTO for the HTTP bootstrap provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::http::HttpBootstrapConfig)]
#[serde(rename_all = "camelCase")]
pub struct HttpBootstrapConfigDto {
    /// Endpoint configurations.
    #[schema(value_type = Vec<bootstrap::http::EndpointConfig>)]
    pub endpoints: Vec<EndpointConfigDto>,

    /// Timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: ConfigValue<u64>,

    /// Maximum retries.
    #[serde(default = "default_retries")]
    pub max_retries: ConfigValue<u32>,

    /// Retry delay in milliseconds.
    #[serde(default = "default_retry_delay")]
    pub retry_delay_ms: ConfigValue<u64>,
}

fn default_timeout() -> ConfigValue<u64> {
    ConfigValue::Static(30)
}

fn default_retries() -> ConfigValue<u32> {
    ConfigValue::Static(3)
}

fn default_retry_delay() -> ConfigValue<u64> {
    ConfigValue::Static(1000)
}

/// Configuration DTO for a single HTTP endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::http::EndpointConfig)]
#[serde(rename_all = "camelCase")]
pub struct EndpointConfigDto {
    /// The URL to fetch data from.
    pub url: ConfigValue<String>,

    /// HTTP method (default: GET).
    #[serde(default = "default_method")]
    #[schema(value_type = bootstrap::http::HttpMethod)]
    pub method: HttpMethodDto,

    /// Additional HTTP headers to include in requests.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, ConfigValue<String>>,

    /// Optional request body (for POST/PUT methods).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,

    /// Authentication configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<bootstrap::http::AuthConfig>)]
    pub auth: Option<AuthConfigDto>,

    /// Pagination configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<bootstrap::http::PaginationConfig>)]
    pub pagination: Option<PaginationConfigDto>,

    /// Response parsing and element mapping configuration.
    #[schema(value_type = bootstrap::http::ResponseConfig)]
    pub response: ResponseConfigDto,
}

fn default_method() -> HttpMethodDto {
    HttpMethodDto::Get
}

/// HTTP methods supported for bootstrap requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::http::HttpMethod)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethodDto {
    Get,
    Post,
    Put,
}

/// Authentication configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::http::AuthConfig)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfigDto {
    /// Bearer token authentication.
    Bearer {
        /// Environment variable containing the token.
        token_env: ConfigValue<String>,
    },
    /// API key authentication (in header or query parameter).
    ApiKey {
        /// Where to send the API key.
        #[schema(value_type = bootstrap::http::ApiKeyLocation)]
        location: ApiKeyLocationDto,
        /// Header name or query parameter name.
        name: ConfigValue<String>,
        /// Environment variable containing the API key value.
        value_env: ConfigValue<String>,
    },
    /// HTTP Basic authentication.
    Basic {
        /// Environment variable containing the username.
        username_env: ConfigValue<String>,
        /// Environment variable containing the password.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password_env: Option<ConfigValue<String>>,
    },
    /// OAuth2 Client Credentials flow.
    #[serde(rename = "oauth2_client_credentials")]
    OAuth2ClientCredentials {
        /// Token endpoint URL.
        token_url: ConfigValue<String>,
        /// Environment variable containing the client ID.
        client_id_env: ConfigValue<String>,
        /// Environment variable containing the client secret.
        client_secret_env: ConfigValue<String>,
        /// Optional scopes to request.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scopes: Vec<ConfigValue<String>>,
    },
}

/// Where to place an API key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::http::ApiKeyLocation)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyLocationDto {
    Header,
    Query,
}

/// Pagination configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::http::PaginationConfig)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaginationConfigDto {
    /// Offset/limit pagination.
    OffsetLimit {
        #[serde(default = "default_offset_param")]
        offset_param: ConfigValue<String>,
        #[serde(default = "default_limit_param")]
        limit_param: ConfigValue<String>,
        page_size: ConfigValue<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_path: Option<ConfigValue<String>>,
    },
    /// Page number pagination.
    PageNumber {
        #[serde(default = "default_page_param")]
        page_param: ConfigValue<String>,
        #[serde(default = "default_per_page_param")]
        page_size_param: ConfigValue<String>,
        page_size: ConfigValue<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_pages_path: Option<ConfigValue<String>>,
    },
    /// Cursor-based pagination.
    Cursor {
        cursor_param: ConfigValue<String>,
        cursor_path: ConfigValue<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        has_more_path: Option<ConfigValue<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page_size_param: Option<ConfigValue<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page_size: Option<ConfigValue<u64>>,
    },
    /// Link header pagination (RFC 5988).
    LinkHeader {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page_size_param: Option<ConfigValue<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page_size: Option<ConfigValue<u64>>,
    },
    /// Next URL from response body.
    NextUrl {
        next_url_path: ConfigValue<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<ConfigValue<String>>,
    },
}

fn default_offset_param() -> ConfigValue<String> {
    ConfigValue::Static("offset".to_string())
}

fn default_limit_param() -> ConfigValue<String> {
    ConfigValue::Static("limit".to_string())
}

fn default_page_param() -> ConfigValue<String> {
    ConfigValue::Static("page".to_string())
}

fn default_per_page_param() -> ConfigValue<String> {
    ConfigValue::Static("per_page".to_string())
}

/// Response parsing configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::http::ResponseConfig)]
#[serde(rename_all = "camelCase")]
pub struct ResponseConfigDto {
    /// JSONPath expression to locate the array of items in the response.
    #[serde(default = "default_items_path")]
    pub items_path: ConfigValue<String>,

    /// Content type override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<bootstrap::http::ContentTypeOverride>)]
    pub content_type: Option<ContentTypeOverrideDto>,

    /// Element mapping configurations.
    #[schema(value_type = Vec<bootstrap::http::ElementMappingConfig>)]
    pub mappings: Vec<ElementMappingConfigDto>,
}

fn default_items_path() -> ConfigValue<String> {
    ConfigValue::Static("$".to_string())
}

/// Content type override DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::http::ContentTypeOverride)]
#[serde(rename_all = "lowercase")]
pub enum ContentTypeOverrideDto {
    Json,
    Xml,
    Yaml,
}

/// Element mapping configuration DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::http::ElementMappingConfig)]
#[serde(rename_all = "camelCase")]
pub struct ElementMappingConfigDto {
    /// Type of element to create.
    #[schema(value_type = bootstrap::http::ElementType)]
    pub element_type: ElementTypeDto,

    /// Template for element creation.
    #[schema(value_type = bootstrap::http::ElementTemplate)]
    pub template: ElementTemplateDto,
}

/// Element type DTO.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::http::ElementType)]
#[serde(rename_all = "lowercase")]
pub enum ElementTypeDto {
    Node,
    Relation,
}

/// Element template DTO using Handlebars expressions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
#[schema(as = bootstrap::http::ElementTemplate)]
#[serde(rename_all = "camelCase")]
pub struct ElementTemplateDto {
    /// Handlebars template for element ID.
    pub id: ConfigValue<String>,

    /// Handlebars templates for element labels.
    pub labels: Vec<ConfigValue<String>>,

    /// Properties mapping (each value is a Handlebars template or literal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,

    /// Template for relation source node ID (relations only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ConfigValue<String>>,

    /// Template for relation target node ID (relations only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<ConfigValue<String>>,
}

// ── Mapping functions ────────────────────────────────────────────────────────

fn map_http_method(dto: &HttpMethodDto) -> HttpMethod {
    match dto {
        HttpMethodDto::Get => HttpMethod::Get,
        HttpMethodDto::Post => HttpMethod::Post,
        HttpMethodDto::Put => HttpMethod::Put,
    }
}

fn map_api_key_location(dto: &ApiKeyLocationDto) -> ApiKeyLocation {
    match dto {
        ApiKeyLocationDto::Header => ApiKeyLocation::Header,
        ApiKeyLocationDto::Query => ApiKeyLocation::Query,
    }
}

fn map_element_type(dto: &ElementTypeDto) -> ElementType {
    match dto {
        ElementTypeDto::Node => ElementType::Node,
        ElementTypeDto::Relation => ElementType::Relation,
    }
}

fn map_content_type_override(dto: &ContentTypeOverrideDto) -> ContentTypeOverride {
    match dto {
        ContentTypeOverrideDto::Json => ContentTypeOverride::Json,
        ContentTypeOverrideDto::Xml => ContentTypeOverride::Xml,
        ContentTypeOverrideDto::Yaml => ContentTypeOverride::Yaml,
    }
}

fn map_auth_config(dto: &AuthConfigDto, resolver: &DtoMapper) -> Result<AuthConfig, MappingError> {
    match dto {
        AuthConfigDto::Bearer { token_env } => Ok(AuthConfig::Bearer {
            token_env: resolver.resolve_string(token_env)?,
        }),
        AuthConfigDto::ApiKey {
            location,
            name,
            value_env,
        } => Ok(AuthConfig::ApiKey {
            location: map_api_key_location(location),
            name: resolver.resolve_string(name)?,
            value_env: resolver.resolve_string(value_env)?,
        }),
        AuthConfigDto::Basic {
            username_env,
            password_env,
        } => Ok(AuthConfig::Basic {
            username_env: resolver.resolve_string(username_env)?,
            password_env: resolver.resolve_optional_string(password_env)?,
        }),
        AuthConfigDto::OAuth2ClientCredentials {
            token_url,
            client_id_env,
            client_secret_env,
            scopes,
        } => Ok(AuthConfig::OAuth2ClientCredentials {
            token_url: resolver.resolve_string(token_url)?,
            client_id_env: resolver.resolve_string(client_id_env)?,
            client_secret_env: resolver.resolve_string(client_secret_env)?,
            scopes: resolver.resolve_string_vec(scopes)?,
        }),
    }
}

fn map_pagination_config(
    dto: &PaginationConfigDto,
    resolver: &DtoMapper,
) -> Result<PaginationConfig, MappingError> {
    match dto {
        PaginationConfigDto::OffsetLimit {
            offset_param,
            limit_param,
            page_size,
            total_path,
        } => Ok(PaginationConfig::OffsetLimit {
            offset_param: resolver.resolve_string(offset_param)?,
            limit_param: resolver.resolve_string(limit_param)?,
            page_size: resolver.resolve_typed(page_size)?,
            total_path: resolver.resolve_optional_string(total_path)?,
        }),
        PaginationConfigDto::PageNumber {
            page_param,
            page_size_param,
            page_size,
            total_pages_path,
        } => Ok(PaginationConfig::PageNumber {
            page_param: resolver.resolve_string(page_param)?,
            page_size_param: resolver.resolve_string(page_size_param)?,
            page_size: resolver.resolve_typed(page_size)?,
            total_pages_path: resolver.resolve_optional_string(total_pages_path)?,
        }),
        PaginationConfigDto::Cursor {
            cursor_param,
            cursor_path,
            has_more_path,
            page_size_param,
            page_size,
        } => Ok(PaginationConfig::Cursor {
            cursor_param: resolver.resolve_string(cursor_param)?,
            cursor_path: resolver.resolve_string(cursor_path)?,
            has_more_path: resolver.resolve_optional_string(has_more_path)?,
            page_size_param: resolver.resolve_optional_string(page_size_param)?,
            page_size: resolver.resolve_optional(page_size)?,
        }),
        PaginationConfigDto::LinkHeader {
            page_size_param,
            page_size,
        } => Ok(PaginationConfig::LinkHeader {
            page_size_param: resolver.resolve_optional_string(page_size_param)?,
            page_size: resolver.resolve_optional(page_size)?,
        }),
        PaginationConfigDto::NextUrl {
            next_url_path,
            base_url,
        } => Ok(PaginationConfig::NextUrl {
            next_url_path: resolver.resolve_string(next_url_path)?,
            base_url: resolver.resolve_optional_string(base_url)?,
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

fn map_element_mapping(
    dto: &ElementMappingConfigDto,
    resolver: &DtoMapper,
) -> Result<ElementMappingConfig, MappingError> {
    Ok(ElementMappingConfig {
        element_type: map_element_type(&dto.element_type),
        template: map_element_template(&dto.template, resolver)?,
    })
}

fn map_response_config(
    dto: &ResponseConfigDto,
    resolver: &DtoMapper,
) -> Result<ResponseConfig, MappingError> {
    Ok(ResponseConfig {
        items_path: resolver.resolve_string(&dto.items_path)?,
        content_type: dto.content_type.as_ref().map(map_content_type_override),
        mappings: dto
            .mappings
            .iter()
            .map(|m| map_element_mapping(m, resolver))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn map_endpoint_config(
    dto: &EndpointConfigDto,
    resolver: &DtoMapper,
) -> Result<EndpointConfig, MappingError> {
    Ok(EndpointConfig {
        url: resolver.resolve_string(&dto.url)?,
        method: map_http_method(&dto.method),
        headers: dto
            .headers
            .iter()
            .map(|(k, v)| Ok((k.clone(), resolver.resolve_string(v)?)))
            .collect::<Result<HashMap<_, _>, MappingError>>()?,
        body: dto.body.clone(),
        auth: dto
            .auth
            .as_ref()
            .map(|a| map_auth_config(a, resolver))
            .transpose()?,
        pagination: dto
            .pagination
            .as_ref()
            .map(|p| map_pagination_config(p, resolver))
            .transpose()?,
        response: map_response_config(&dto.response, resolver)?,
    })
}

fn map_config(
    dto: &HttpBootstrapConfigDto,
    resolver: &DtoMapper,
) -> Result<HttpBootstrapConfig, MappingError> {
    Ok(HttpBootstrapConfig {
        endpoints: dto
            .endpoints
            .iter()
            .map(|e| map_endpoint_config(e, resolver))
            .collect::<Result<Vec<_>, _>>()?,
        timeout_seconds: resolver.resolve_typed(&dto.timeout_seconds)?,
        max_retries: resolver.resolve_typed(&dto.max_retries)?,
        retry_delay_ms: resolver.resolve_typed(&dto.retry_delay_ms)?,
    })
}

// ── OpenAPI schema registration ─────────────────────────────────────────────

#[derive(OpenApi)]
#[openapi(components(schemas(
    HttpBootstrapConfigDto,
    EndpointConfigDto,
    HttpMethodDto,
    AuthConfigDto,
    ApiKeyLocationDto,
    PaginationConfigDto,
    ResponseConfigDto,
    ContentTypeOverrideDto,
    ElementMappingConfigDto,
    ElementTypeDto,
    ElementTemplateDto,
)))]
struct HttpBootstrapSchemas;

// ── Descriptor ──────────────────────────────────────────────────────────────

/// Plugin descriptor for the HTTP bootstrap provider.
pub struct HttpBootstrapDescriptor;

#[async_trait]
impl BootstrapPluginDescriptor for HttpBootstrapDescriptor {
    fn kind(&self) -> &str {
        "http"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "bootstrap.http.HttpBootstrapConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = HttpBootstrapSchemas::openapi();
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
        _source_config_json: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn BootstrapProvider>> {
        let dto: HttpBootstrapConfigDto = serde_json::from_value(config_json.clone())
            .map_err(|e| anyhow::anyhow!("Failed to parse HTTP bootstrap config: {e}"))?;

        let mapper = DtoMapper::new();
        let config = map_config(&dto, &mapper)
            .map_err(|e| anyhow::anyhow!("Failed to resolve HTTP bootstrap config: {e}"))?;

        let provider = HttpBootstrapProvider::new(config)?;
        Ok(Box::new(provider))
    }
}
