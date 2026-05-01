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

//! Configuration types for the HTTP bootstrap provider.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Top-level configuration for the HTTP bootstrap provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HttpBootstrapConfig {
    /// List of endpoint configurations to fetch data from.
    pub endpoints: Vec<EndpointConfig>,

    /// HTTP request timeout in seconds (default: 30).
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,

    /// Maximum number of retries on failure (default: 3).
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Delay between retries in milliseconds (default: 1000).
    #[serde(default = "default_retry_delay_ms")]
    pub retry_delay_ms: u64,
}

fn default_timeout_seconds() -> u64 {
    30
}

fn default_max_retries() -> u32 {
    3
}

fn default_retry_delay_ms() -> u64 {
    1000
}

/// Configuration for a single HTTP endpoint to bootstrap from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EndpointConfig {
    /// The URL to fetch data from.
    pub url: String,

    /// HTTP method (default: GET).
    #[serde(default = "default_method")]
    pub method: HttpMethod,

    /// Additional HTTP headers to include in requests.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,

    /// Optional request body (for POST/PUT methods).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,

    /// Authentication configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,

    /// Pagination configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pagination: Option<PaginationConfig>,

    /// Response parsing and element mapping configuration.
    pub response: ResponseConfig,
}

fn default_method() -> HttpMethod {
    HttpMethod::Get
}

/// HTTP methods supported for bootstrap requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
}

/// Authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    /// Bearer token authentication.
    Bearer {
        /// Environment variable containing the token.
        token_env: String,
    },
    /// API key authentication (in header or query parameter).
    ApiKey {
        /// Where to send the API key.
        location: ApiKeyLocation,
        /// Header name or query parameter name.
        name: String,
        /// Environment variable containing the API key value.
        value_env: String,
    },
    /// HTTP Basic authentication.
    Basic {
        /// Environment variable containing the username.
        username_env: String,
        /// Environment variable containing the password (optional, can be empty).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password_env: Option<String>,
    },
    /// OAuth2 Client Credentials flow.
    #[serde(rename = "oauth2_client_credentials")]
    OAuth2ClientCredentials {
        /// Token endpoint URL.
        token_url: String,
        /// Environment variable containing the client ID.
        client_id_env: String,
        /// Environment variable containing the client secret.
        client_secret_env: String,
        /// Optional scopes to request.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scopes: Vec<String>,
    },
}

/// Where to place an API key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyLocation {
    Header,
    Query,
}

/// Pagination configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaginationConfig {
    /// Offset/limit pagination.
    OffsetLimit {
        /// Query parameter name for offset (default: "offset").
        #[serde(default = "default_offset_param")]
        offset_param: String,
        /// Query parameter name for limit/page size (default: "limit").
        #[serde(default = "default_limit_param")]
        limit_param: String,
        /// Number of items per page.
        page_size: u64,
        /// JSONPath to extract total count from response (optional).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_path: Option<String>,
    },
    /// Page number pagination.
    PageNumber {
        /// Query parameter name for page number (default: "page").
        #[serde(default = "default_page_param")]
        page_param: String,
        /// Query parameter name for page size (default: "per_page").
        #[serde(default = "default_per_page_param")]
        page_size_param: String,
        /// Number of items per page.
        page_size: u64,
        /// JSONPath to extract total pages from response (optional).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_pages_path: Option<String>,
    },
    /// Cursor-based pagination (e.g., Stripe's `starting_after`).
    Cursor {
        /// Query parameter name to send the cursor value.
        cursor_param: String,
        /// JSONPath to extract the next cursor value from the response.
        cursor_path: String,
        /// JSONPath to a boolean `has_more` field (optional).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        has_more_path: Option<String>,
        /// Query parameter name for page size (optional).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page_size_param: Option<String>,
        /// Number of items per page (optional).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page_size: Option<u64>,
    },
    /// Link header pagination (RFC 5988).
    LinkHeader {
        /// Query parameter name for page size (optional).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page_size_param: Option<String>,
        /// Number of items per page (optional).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        page_size: Option<u64>,
    },
    /// Next URL from response body (e.g., Salesforce `nextRecordsUrl`).
    NextUrl {
        /// JSONPath to extract the next URL from the response body.
        next_url_path: String,
        /// Base URL to prepend if the extracted URL is relative.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
    },
}

fn default_offset_param() -> String {
    "offset".to_string()
}

fn default_limit_param() -> String {
    "limit".to_string()
}

fn default_page_param() -> String {
    "page".to_string()
}

fn default_per_page_param() -> String {
    "per_page".to_string()
}

/// Response parsing configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResponseConfig {
    /// JSONPath expression to locate the array of items in the response.
    /// Use "$" if the response is a top-level array.
    #[serde(default = "default_items_path")]
    pub items_path: String,

    /// Content type override (auto-detected from Content-Type header if not set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<ContentTypeOverride>,

    /// Element mapping configurations.
    pub mappings: Vec<ElementMappingConfig>,
}

fn default_items_path() -> String {
    "$".to_string()
}

/// Override for response content type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ContentTypeOverride {
    Json,
    Xml,
    Yaml,
}

/// Mapping configuration from response items to Drasi graph elements.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ElementMappingConfig {
    /// Type of element to create.
    pub element_type: ElementType,

    /// Template for element creation.
    pub template: ElementTemplate,
}

/// Element type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ElementType {
    Node,
    Relation,
}

/// Template for element creation using Handlebars expressions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ElementTemplate {
    /// Handlebars template for element ID.
    pub id: String,

    /// Handlebars templates for element labels.
    pub labels: Vec<String>,

    /// Properties mapping (each value is a Handlebars template or literal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,

    /// Template for relation source node ID (relations only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,

    /// Template for relation target node ID (relations only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_full_config() {
        let json = r#"{
            "endpoints": [{
                "url": "https://api.example.com/users",
                "method": "GET",
                "auth": {
                    "type": "bearer",
                    "token_env": "API_TOKEN"
                },
                "pagination": {
                    "type": "offset_limit",
                    "offset_param": "offset",
                    "limit_param": "limit",
                    "page_size": 100
                },
                "response": {
                    "itemsPath": "$.data",
                    "mappings": [{
                        "elementType": "node",
                        "template": {
                            "id": "{{item.id}}",
                            "labels": ["User"],
                            "properties": {
                                "name": "{{item.name}}"
                            }
                        }
                    }]
                }
            }],
            "timeoutSeconds": 30,
            "maxRetries": 3,
            "retryDelayMs": 1000
        }"#;

        let config: HttpBootstrapConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.endpoints.len(), 1);
        assert_eq!(config.timeout_seconds, 30);
        assert_eq!(config.endpoints[0].url, "https://api.example.com/users");
    }

    #[test]
    fn test_deserialize_cursor_pagination() {
        let json = r#"{
            "type": "cursor",
            "cursor_param": "starting_after",
            "cursor_path": "$.data[-1].id",
            "has_more_path": "$.has_more",
            "page_size_param": "limit",
            "page_size": 100
        }"#;

        let config: PaginationConfig = serde_json::from_str(json).unwrap();
        match config {
            PaginationConfig::Cursor {
                cursor_param,
                cursor_path,
                has_more_path,
                ..
            } => {
                assert_eq!(cursor_param, "starting_after");
                assert_eq!(cursor_path, "$.data[-1].id");
                assert_eq!(has_more_path, Some("$.has_more".to_string()));
            }
            _ => panic!("Expected Cursor pagination"),
        }
    }

    #[test]
    fn test_deserialize_oauth2_auth() {
        let json = r#"{
            "type": "oauth2_client_credentials",
            "token_url": "https://auth.example.com/token",
            "client_id_env": "CLIENT_ID",
            "client_secret_env": "CLIENT_SECRET",
            "scopes": ["read", "write"]
        }"#;

        let config: AuthConfig = serde_json::from_str(json).unwrap();
        match config {
            AuthConfig::OAuth2ClientCredentials {
                token_url,
                scopes,
                ..
            } => {
                assert_eq!(token_url, "https://auth.example.com/token");
                assert_eq!(scopes, vec!["read", "write"]);
            }
            _ => panic!("Expected OAuth2ClientCredentials"),
        }
    }

    #[test]
    fn test_deserialize_next_url_pagination() {
        let json = r#"{
            "type": "next_url",
            "next_url_path": "$.nextRecordsUrl",
            "base_url": "https://instance.salesforce.com"
        }"#;

        let config: PaginationConfig = serde_json::from_str(json).unwrap();
        match config {
            PaginationConfig::NextUrl {
                next_url_path,
                base_url,
            } => {
                assert_eq!(next_url_path, "$.nextRecordsUrl");
                assert_eq!(
                    base_url,
                    Some("https://instance.salesforce.com".to_string())
                );
            }
            _ => panic!("Expected NextUrl pagination"),
        }
    }
}
