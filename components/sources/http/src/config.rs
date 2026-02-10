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

//! Configuration for the HTTP source.
//!
//! The HTTP source receives data changes via HTTP endpoints.
//! It supports two mutually exclusive modes:
//!
//! - **Standard Mode**: Uses the built-in `HttpSourceChange` format
//! - **Webhook Mode**: Custom routes with configurable payload mappings

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// HTTP source configuration
///
/// This config only contains HTTP-specific settings.
/// Bootstrap provider configuration (database, user, password, tables, etc.)
/// should be provided via the source's generic properties map.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpSourceConfig {
    /// HTTP host
    pub host: String,

    /// HTTP port
    pub port: u16,

    /// Optional endpoint path
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Adaptive batching: maximum batch size
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_batch_size: Option<usize>,

    /// Adaptive batching: minimum batch size
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_batch_size: Option<usize>,

    /// Adaptive batching: maximum wait time in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_wait_ms: Option<u64>,

    /// Adaptive batching: minimum wait time in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_wait_ms: Option<u64>,

    /// Adaptive batching: throughput window in seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_window_secs: Option<u64>,

    /// Whether adaptive batching is enabled
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_enabled: Option<bool>,

    /// Webhook configuration (enables webhook mode when present)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhooks: Option<WebhookConfig>,
}

/// Returns true if webhook mode is enabled
impl HttpSourceConfig {
    /// Check if webhook mode is enabled
    pub fn is_webhook_mode(&self) -> bool {
        self.webhooks.is_some()
    }
}

/// Webhook configuration for custom route handling
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookConfig {
    /// Global error behavior for unmatched/failed requests
    #[serde(default)]
    pub error_behavior: ErrorBehavior,

    /// CORS (Cross-Origin Resource Sharing) configuration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,

    /// List of webhook route configurations
    pub routes: Vec<WebhookRoute>,
}

/// CORS configuration for webhook endpoints
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CorsConfig {
    /// Whether CORS is enabled (default: true when cors section is present)
    #[serde(default = "default_cors_enabled")]
    pub enabled: bool,

    /// Allowed origins. Use ["*"] for any origin, or specific origins like ["https://example.com"]
    #[serde(default = "default_cors_origins")]
    pub allow_origins: Vec<String>,

    /// Allowed HTTP methods
    #[serde(default = "default_cors_methods")]
    pub allow_methods: Vec<String>,

    /// Allowed headers. Use ["*"] for any header.
    #[serde(default = "default_cors_headers")]
    pub allow_headers: Vec<String>,

    /// Headers to expose to the browser
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expose_headers: Vec<String>,

    /// Whether to allow credentials (cookies, authorization headers)
    #[serde(default)]
    pub allow_credentials: bool,

    /// Max age in seconds for preflight request caching
    #[serde(default = "default_cors_max_age")]
    pub max_age: u64,
}

fn default_cors_enabled() -> bool {
    true
}

fn default_cors_origins() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_cors_methods() -> Vec<String> {
    vec![
        "GET".to_string(),
        "POST".to_string(),
        "PUT".to_string(),
        "PATCH".to_string(),
        "DELETE".to_string(),
        "OPTIONS".to_string(),
    ]
}

fn default_cors_headers() -> Vec<String> {
    vec![
        "Content-Type".to_string(),
        "Authorization".to_string(),
        "X-Requested-With".to_string(),
    ]
}

fn default_cors_max_age() -> u64 {
    3600
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_origins: default_cors_origins(),
            allow_methods: default_cors_methods(),
            allow_headers: default_cors_headers(),
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age: default_cors_max_age(),
        }
    }
}

/// Error handling behavior for webhook requests
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ErrorBehavior {
    /// Accept the request and log the issue (returns 200)
    #[default]
    AcceptAndLog,
    /// Accept the request but silently discard (returns 200)
    AcceptAndSkip,
    /// Reject the request with an appropriate HTTP error
    Reject,
}

/// Configuration for a single webhook route
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookRoute {
    /// Route path pattern (supports `:param` for path parameters)
    /// Example: "/github/events" or "/users/:user_id/webhooks"
    pub path: String,

    /// Allowed HTTP methods for this route
    #[serde(default = "default_methods")]
    pub methods: Vec<HttpMethod>,

    /// Authentication configuration
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,

    /// Error behavior override for this route
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_behavior: Option<ErrorBehavior>,

    /// Mappings from payload to source change events
    pub mappings: Vec<WebhookMapping>,
}

fn default_methods() -> Vec<HttpMethod> {
    vec![HttpMethod::Post]
}

/// HTTP methods supported for webhook routes
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// Authentication configuration for a webhook route
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthConfig {
    /// HMAC signature verification
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<SignatureConfig>,

    /// Bearer token verification
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer: Option<BearerConfig>,
}

/// HMAC signature verification configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignatureConfig {
    /// Signature algorithm type
    #[serde(rename = "type")]
    pub algorithm: SignatureAlgorithm,

    /// Environment variable containing the secret
    pub secret_env: String,

    /// Header containing the signature
    pub header: String,

    /// Prefix to strip from signature (e.g., "sha256=")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,

    /// Encoding of the signature (hex or base64)
    #[serde(default)]
    pub encoding: SignatureEncoding,
}

/// Supported HMAC signature algorithms
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum SignatureAlgorithm {
    HmacSha1,
    HmacSha256,
}

/// Signature encoding format
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SignatureEncoding {
    #[default]
    Hex,
    Base64,
}

/// Bearer token verification configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BearerConfig {
    /// Environment variable containing the expected token
    pub token_env: String,
}

/// Mapping configuration from webhook payload to source change event
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookMapping {
    /// Optional condition for when this mapping applies
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<MappingCondition>,

    /// Static operation type
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<OperationType>,

    /// Path to extract operation from payload
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_from: Option<String>,

    /// Mapping from payload values to operation types
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_map: Option<HashMap<String, OperationType>>,

    /// Element type to create
    pub element_type: ElementType,

    /// Timestamp configuration for effective_from
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_from: Option<EffectiveFromConfig>,

    /// Template for element creation
    pub template: ElementTemplate,
}

/// Condition for when a mapping applies
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MappingCondition {
    /// Header to check
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,

    /// Payload field path to check
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,

    /// Value must equal this
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<String>,

    /// Value must contain this
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,

    /// Value must match this regex
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regex: Option<String>,
}

/// Operation type for source changes
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OperationType {
    Insert,
    Update,
    Delete,
}

/// Element type for source changes
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ElementType {
    Node,
    Relation,
}

/// Configuration for effective_from timestamp
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum EffectiveFromConfig {
    /// Simple template string (auto-detect format)
    Simple(String),
    /// Explicit configuration with format
    Explicit {
        /// Template for the timestamp value
        value: String,
        /// Format of the timestamp
        format: TimestampFormat,
    },
}

/// Timestamp format for effective_from
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TimestampFormat {
    /// ISO 8601 datetime string
    Iso8601,
    /// Unix timestamp in seconds
    UnixSeconds,
    /// Unix timestamp in milliseconds
    UnixMillis,
    /// Unix timestamp in nanoseconds
    UnixNanos,
}

/// Template for element creation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ElementTemplate {
    /// Template for element ID
    pub id: String,

    /// Templates for element labels
    pub labels: Vec<String>,

    /// Templates for element properties (can be individual templates or a single object template)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<serde_json::Value>,

    /// Template for relation source node ID (relations only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,

    /// Template for relation target node ID (relations only)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

fn default_timeout_ms() -> u64 {
    10000
}

impl HttpSourceConfig {
    /// Validate the configuration and return an error if invalid.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Port is 0 (invalid port)
    /// - Timeout is 0 (would cause immediate timeouts)
    /// - Adaptive batching min values exceed max values
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.port == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: port cannot be 0. \
                 Please specify a valid port number (1-65535)"
            ));
        }

        if self.timeout_ms == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: timeout_ms cannot be 0. \
                 Please specify a positive timeout value in milliseconds"
            ));
        }

        // Validate adaptive batching settings
        if let (Some(min), Some(max)) = (self.adaptive_min_batch_size, self.adaptive_max_batch_size)
        {
            if min > max {
                return Err(anyhow::anyhow!(
                    "Validation error: adaptive_min_batch_size ({min}) cannot be greater than \
                     adaptive_max_batch_size ({max})"
                ));
            }
        }

        if let (Some(min), Some(max)) = (self.adaptive_min_wait_ms, self.adaptive_max_wait_ms) {
            if min > max {
                return Err(anyhow::anyhow!(
                    "Validation error: adaptive_min_wait_ms ({min}) cannot be greater than \
                     adaptive_max_wait_ms ({max})"
                ));
            }
        }

        // Validate webhook configuration if present
        if let Some(ref webhooks) = self.webhooks {
            webhooks.validate()?;
        }

        Ok(())
    }
}

impl WebhookConfig {
    /// Validate webhook configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.routes.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: webhooks.routes cannot be empty"
            ));
        }

        for (idx, route) in self.routes.iter().enumerate() {
            route
                .validate()
                .map_err(|e| anyhow::anyhow!("Validation error in route[{idx}]: {e}"))?;
        }

        Ok(())
    }
}

impl WebhookRoute {
    /// Validate webhook route configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.path.is_empty() {
            return Err(anyhow::anyhow!("path cannot be empty"));
        }

        if !self.path.starts_with('/') {
            return Err(anyhow::anyhow!("path must start with '/'"));
        }

        if self.methods.is_empty() {
            return Err(anyhow::anyhow!("methods cannot be empty"));
        }

        if self.mappings.is_empty() {
            return Err(anyhow::anyhow!("mappings cannot be empty"));
        }

        for (idx, mapping) in self.mappings.iter().enumerate() {
            mapping
                .validate()
                .map_err(|e| anyhow::anyhow!("mappings[{idx}]: {e}"))?;
        }

        Ok(())
    }
}

impl WebhookMapping {
    /// Validate webhook mapping configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        // Must have either static operation or dynamic operation_from
        if self.operation.is_none() && self.operation_from.is_none() {
            return Err(anyhow::anyhow!(
                "either 'operation' or 'operation_from' must be specified"
            ));
        }

        // If using operation_from, should have operation_map
        if self.operation_from.is_some() && self.operation_map.is_none() {
            return Err(anyhow::anyhow!(
                "'operation_map' is required when using 'operation_from'"
            ));
        }

        // Validate template
        self.template.validate(&self.element_type)?;

        Ok(())
    }
}

impl ElementTemplate {
    /// Validate element template configuration
    pub fn validate(&self, element_type: &ElementType) -> anyhow::Result<()> {
        if self.id.is_empty() {
            return Err(anyhow::anyhow!("template.id cannot be empty"));
        }

        if self.labels.is_empty() {
            return Err(anyhow::anyhow!("template.labels cannot be empty"));
        }

        // Relations require from and to
        if *element_type == ElementType::Relation {
            if self.from.is_none() {
                return Err(anyhow::anyhow!(
                    "template.from is required for relation elements"
                ));
            }
            if self.to.is_none() {
                return Err(anyhow::anyhow!(
                    "template.to is required for relation elements"
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_deserialization_minimal() {
        let yaml = r#"
host: "localhost"
port: 8080
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 8080);
        assert_eq!(config.endpoint, None);
        assert_eq!(config.timeout_ms, 10000); // default
        assert_eq!(config.adaptive_enabled, None);
    }

    #[test]
    fn test_config_deserialization_full() {
        let yaml = r#"
host: "0.0.0.0"
port: 9000
endpoint: "/events"
timeout_ms: 5000
adaptive_max_batch_size: 1000
adaptive_min_batch_size: 10
adaptive_max_wait_ms: 500
adaptive_min_wait_ms: 10
adaptive_window_secs: 60
adaptive_enabled: true
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 9000);
        assert_eq!(config.endpoint, Some("/events".to_string()));
        assert_eq!(config.timeout_ms, 5000);
        assert_eq!(config.adaptive_max_batch_size, Some(1000));
        assert_eq!(config.adaptive_min_batch_size, Some(10));
        assert_eq!(config.adaptive_max_wait_ms, Some(500));
        assert_eq!(config.adaptive_min_wait_ms, Some(10));
        assert_eq!(config.adaptive_window_secs, Some(60));
        assert_eq!(config.adaptive_enabled, Some(true));
    }

    #[test]
    fn test_config_serialization() {
        let config = HttpSourceConfig {
            host: "localhost".to_string(),
            port: 8080,
            endpoint: Some("/data".to_string()),
            timeout_ms: 15000,
            adaptive_max_batch_size: Some(500),
            adaptive_min_batch_size: Some(5),
            adaptive_max_wait_ms: Some(1000),
            adaptive_min_wait_ms: Some(50),
            adaptive_window_secs: Some(30),
            adaptive_enabled: Some(false),
            webhooks: None,
        };

        let yaml = serde_yaml::to_string(&config).unwrap();
        let deserialized: HttpSourceConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_config_adaptive_batching_disabled() {
        let yaml = r#"
host: "localhost"
port: 8080
adaptive_enabled: false
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.adaptive_enabled, Some(false));
    }

    #[test]
    fn test_config_default_values() {
        let config = HttpSourceConfig {
            host: "localhost".to_string(),
            port: 8080,
            endpoint: None,
            timeout_ms: default_timeout_ms(),
            adaptive_max_batch_size: None,
            adaptive_min_batch_size: None,
            adaptive_max_wait_ms: None,
            adaptive_min_wait_ms: None,
            adaptive_window_secs: None,
            adaptive_enabled: None,
            webhooks: None,
        };

        assert_eq!(config.timeout_ms, 10000);
    }

    #[test]
    fn test_config_port_range() {
        // Test valid port
        let yaml = r#"
host: "localhost"
port: 65535
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.port, 65535);

        // Test minimum port
        let yaml = r#"
host: "localhost"
port: 1
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.port, 1);
    }

    #[test]
    fn test_webhook_config_deserialization() {
        let yaml = r#"
host: "0.0.0.0"
port: 8080
webhooks:
  error_behavior: reject
  routes:
    - path: "/github/events"
      methods: ["POST"]
      auth:
        signature:
          type: hmac-sha256
          secret_env: GITHUB_SECRET
          header: X-Hub-Signature-256
          prefix: "sha256="
        bearer:
          token_env: GITHUB_TOKEN
      error_behavior: reject
      mappings:
        - when:
            header: X-GitHub-Event
            equals: push
          operation: insert
          element_type: node
          effective_from: "{{payload.timestamp}}"
          template:
            id: "commit-{{payload.id}}"
            labels: ["Commit"]
            properties:
              message: "{{payload.message}}"
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.webhooks.is_some());
        let webhooks = config.webhooks.unwrap();
        assert_eq!(webhooks.error_behavior, ErrorBehavior::Reject);
        assert_eq!(webhooks.routes.len(), 1);

        let route = &webhooks.routes[0];
        assert_eq!(route.path, "/github/events");
        assert_eq!(route.methods, vec![HttpMethod::Post]);
        assert!(route.auth.is_some());

        let auth = route.auth.as_ref().unwrap();
        assert!(auth.signature.is_some());
        assert!(auth.bearer.is_some());

        let sig = auth.signature.as_ref().unwrap();
        assert_eq!(sig.algorithm, SignatureAlgorithm::HmacSha256);
        assert_eq!(sig.secret_env, "GITHUB_SECRET");
        assert_eq!(sig.header, "X-Hub-Signature-256");
        assert_eq!(sig.prefix, Some("sha256=".to_string()));

        let mapping = &route.mappings[0];
        assert!(mapping.when.is_some());
        assert_eq!(mapping.operation, Some(OperationType::Insert));
        assert_eq!(mapping.element_type, ElementType::Node);
    }

    #[test]
    fn test_webhook_config_with_operation_map() {
        let yaml = r#"
host: "0.0.0.0"
port: 8080
webhooks:
  routes:
    - path: "/events"
      mappings:
        - operation_from: "payload.action"
          operation_map:
            created: insert
            updated: update
            deleted: delete
          element_type: node
          template:
            id: "{{payload.id}}"
            labels: ["Event"]
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        let webhooks = config.webhooks.unwrap();
        let mapping = &webhooks.routes[0].mappings[0];

        assert_eq!(mapping.operation_from, Some("payload.action".to_string()));
        assert!(mapping.operation_map.is_some());
        let op_map = mapping.operation_map.as_ref().unwrap();
        assert_eq!(op_map.get("created"), Some(&OperationType::Insert));
        assert_eq!(op_map.get("updated"), Some(&OperationType::Update));
        assert_eq!(op_map.get("deleted"), Some(&OperationType::Delete));
    }

    #[test]
    fn test_webhook_config_relation() {
        let yaml = r#"
host: "0.0.0.0"
port: 8080
webhooks:
  routes:
    - path: "/links"
      mappings:
        - operation: insert
          element_type: relation
          template:
            id: "{{payload.id}}"
            labels: ["LINKS_TO"]
            from: "{{payload.source_id}}"
            to: "{{payload.target_id}}"
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        let mapping = &config.webhooks.unwrap().routes[0].mappings[0];
        assert_eq!(mapping.element_type, ElementType::Relation);
        assert_eq!(
            mapping.template.from,
            Some("{{payload.source_id}}".to_string())
        );
        assert_eq!(
            mapping.template.to,
            Some("{{payload.target_id}}".to_string())
        );
    }

    #[test]
    fn test_effective_from_simple() {
        let yaml = r#"
host: "0.0.0.0"
port: 8080
webhooks:
  routes:
    - path: "/events"
      mappings:
        - operation: insert
          element_type: node
          effective_from: "{{payload.timestamp}}"
          template:
            id: "{{payload.id}}"
            labels: ["Event"]
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        let mapping = &config.webhooks.unwrap().routes[0].mappings[0];
        assert_eq!(
            mapping.effective_from,
            Some(EffectiveFromConfig::Simple(
                "{{payload.timestamp}}".to_string()
            ))
        );
    }

    #[test]
    fn test_effective_from_explicit() {
        let yaml = r#"
host: "0.0.0.0"
port: 8080
webhooks:
  routes:
    - path: "/events"
      mappings:
        - operation: insert
          element_type: node
          effective_from:
            value: "{{payload.created_at}}"
            format: iso8601
          template:
            id: "{{payload.id}}"
            labels: ["Event"]
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        let mapping = &config.webhooks.unwrap().routes[0].mappings[0];
        match &mapping.effective_from {
            Some(EffectiveFromConfig::Explicit { value, format }) => {
                assert_eq!(value, "{{payload.created_at}}");
                assert_eq!(*format, TimestampFormat::Iso8601);
            }
            _ => panic!("Expected explicit effective_from config"),
        }
    }

    #[test]
    fn test_is_webhook_mode() {
        let yaml_standard = r#"
host: "localhost"
port: 8080
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml_standard).unwrap();
        assert!(!config.is_webhook_mode());

        let yaml_webhook = r#"
host: "localhost"
port: 8080
webhooks:
  routes:
    - path: "/events"
      mappings:
        - operation: insert
          element_type: node
          template:
            id: "{{payload.id}}"
            labels: ["Event"]
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml_webhook).unwrap();
        assert!(config.is_webhook_mode());
    }

    #[test]
    fn test_webhook_validation_empty_routes() {
        let yaml = r#"
host: "localhost"
port: 8080
webhooks:
  routes: []
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_webhook_validation_missing_operation() {
        let yaml = r#"
host: "localhost"
port: 8080
webhooks:
  routes:
    - path: "/events"
      mappings:
        - element_type: node
          template:
            id: "{{payload.id}}"
            labels: ["Event"]
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_webhook_validation_relation_missing_from_to() {
        let yaml = r#"
host: "localhost"
port: 8080
webhooks:
  routes:
    - path: "/events"
      mappings:
        - operation: insert
          element_type: relation
          template:
            id: "{{payload.id}}"
            labels: ["LINKS"]
"#;
        let config: HttpSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.validate().is_err());
    }
}
