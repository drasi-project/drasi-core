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

use drasi_lib::DurabilityConfig;

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

    /// Optional WAL durability configuration.
    ///
    /// When present and enabled, the source persists incoming events to a
    /// local Write-Ahead Log before acknowledging the caller, enabling
    /// crash recovery and replay for persistent queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durability: Option<DurabilityConfig>,
}

/// Returns true if webhook mode is enabled
impl HttpSourceConfig {
    /// Check if webhook mode is enabled
    pub fn is_webhook_mode(&self) -> bool {
        self.webhooks.is_some()
    }
}

pub use drasi_http_common::webhook::config::*;

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
            durability: None,
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
            durability: None,
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
