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

//! Bootstrap provider architecture for Drasi
//!
//! This module provides a pluggable bootstrap system that separates bootstrap
//! concerns from source streaming logic. Bootstrap providers can be reused
//! across different source types while maintaining access to their parent
//! source configuration.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Request for bootstrap data from a query
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapRequest {
    pub query_id: String,
    pub node_labels: Vec<String>,
    pub relation_labels: Vec<String>,
    pub request_id: String,
}

/// Context passed to bootstrap providers
/// Bootstrap happens through dedicated channels created in source.subscribe().
///
/// # Plugin Architecture
///
/// BootstrapContext no longer contains source configuration. Bootstrap providers
/// are created by source plugins using their own typed configurations. This context
/// provides only the minimal information needed during bootstrap execution:
/// - Source identification
/// - Sequence numbering for events
/// - Optional properties for providers that need runtime data
#[derive(Clone)]
pub struct BootstrapContext {
    /// Unique server ID for logging and tracing
    pub server_id: String,
    /// Source ID for labeling bootstrap events
    pub source_id: String,
    /// Sequence counter for bootstrap events
    pub sequence_counter: Arc<AtomicU64>,
    /// Optional properties that can be set by plugins if needed
    properties: Arc<HashMap<String, serde_json::Value>>,
}

impl BootstrapContext {
    /// Create a minimal bootstrap context with just server and source IDs
    ///
    /// This is the preferred constructor. Bootstrap providers should have their
    /// own configuration - they don't need access to source config.
    pub fn new_minimal(server_id: String, source_id: String) -> Self {
        Self {
            server_id,
            source_id,
            sequence_counter: Arc::new(AtomicU64::new(0)),
            properties: Arc::new(HashMap::new()),
        }
    }

    /// Create a bootstrap context with properties
    ///
    /// Use this if your bootstrap provider needs access to some properties
    /// at runtime. The properties should be extracted from your plugin's
    /// typed configuration.
    pub fn with_properties(
        server_id: String,
        source_id: String,
        properties: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            server_id,
            source_id,
            sequence_counter: Arc::new(AtomicU64::new(0)),
            properties: Arc::new(properties),
        }
    }

    /// Get the next sequence number for bootstrap events
    pub fn next_sequence(&self) -> u64 {
        self.sequence_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Get a property from the context
    pub fn get_property(&self, key: &str) -> Option<serde_json::Value> {
        self.properties.get(key).cloned()
    }

    /// Get a typed property from the context
    pub fn get_typed_property<T>(&self, key: &str) -> Result<Option<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        match self.get_property(key) {
            Some(value) => Ok(Some(serde_json::from_value(value.clone())?)),
            None => Ok(None),
        }
    }
}

use crate::channels::BootstrapEventSender;

/// Trait for bootstrap providers that handle initial data delivery
/// for newly subscribed queries
#[async_trait]
pub trait BootstrapProvider: Send + Sync {
    /// Perform bootstrap operation for the given request
    /// Sends bootstrap events to the provided channel
    /// Returns the number of elements sent
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
    ) -> Result<usize>;
}

/// Blanket implementation of BootstrapProvider for boxed trait objects.
/// This allows Box<dyn BootstrapProvider> to be used where BootstrapProvider is expected.
#[async_trait]
impl BootstrapProvider for Box<dyn BootstrapProvider> {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
    ) -> Result<usize> {
        (**self).bootstrap(request, context, event_tx).await
    }
}

// Typed configuration structs for each bootstrap provider type

/// PostgreSQL bootstrap provider configuration
///
/// This provider bootstraps initial data from a PostgreSQL database using
/// the COPY protocol for efficient data transfer. The provider extracts
/// connection details from the parent source configuration, so no additional
/// configuration is needed here.
///
/// # Example
/// ```yaml
/// bootstrap_provider:
///   type: postgres
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PostgresBootstrapConfig {
    // No additional config needed - uses parent source config
    // Include this struct for consistency and future extensibility
}

/// Application bootstrap provider configuration
///
/// This provider bootstraps data from in-memory storage maintained by
/// application sources. It replays stored insert events to provide initial
/// data. No additional configuration is required as it uses shared state
/// from the application source.
///
/// # Example
/// ```yaml
/// bootstrap_provider:
///   type: application
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ApplicationBootstrapConfig {
    // No config needed - uses shared state
    // Include for consistency and future extensibility
}

/// Script file bootstrap provider configuration
///
/// This provider reads bootstrap data from JSONL (JSON Lines) files containing
/// structured data. Files are processed in the order specified. Each file should
/// contain one JSON record per line with record types: Header (required first),
/// Node, Relation, Comment (filtered), Label (checkpoint), and Finish (optional end).
///
/// # Example
/// ```yaml
/// bootstrap_provider:
///   type: scriptfile
///   file_paths:
///     - "/data/initial_nodes.jsonl"
///     - "/data/initial_relations.jsonl"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScriptFileBootstrapConfig {
    /// List of JSONL files to read (in order)
    pub file_paths: Vec<String>,
}

/// Platform bootstrap provider configuration
///
/// This provider bootstraps data from a Query API service running in a remote
/// Drasi environment via HTTP streaming. It's designed for cross-Drasi integration
/// scenarios where one Drasi instance needs initial data from another.
///
/// # Example
/// ```yaml
/// bootstrap_provider:
///   type: platform
///   query_api_url: "http://remote-drasi:8080"
///   timeout_seconds: 600
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlatformBootstrapConfig {
    /// URL of the Query API service (e.g., "http://my-source-query-api:8080")
    /// If not specified, falls back to `query_api_url` property from source config
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_api_url: Option<String>,

    /// Timeout for HTTP requests in seconds (default: 300)
    #[serde(default = "default_platform_timeout")]
    pub timeout_seconds: u64,
}

fn default_platform_timeout() -> u64 {
    300
}

impl Default for PlatformBootstrapConfig {
    fn default() -> Self {
        Self {
            query_api_url: None,
            timeout_seconds: default_platform_timeout(),
        }
    }
}

/// Configuration for different types of bootstrap providers
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BootstrapProviderConfig {
    /// PostgreSQL bootstrap provider
    Postgres(PostgresBootstrapConfig),
    /// Application-based bootstrap provider
    Application(ApplicationBootstrapConfig),
    /// Script file bootstrap provider
    ScriptFile(ScriptFileBootstrapConfig),
    /// Platform bootstrap provider for remote Drasi sources
    /// Bootstraps data from a Query API service running in a remote Drasi environment
    Platform(PlatformBootstrapConfig),
    /// No-op bootstrap provider (returns no data)
    Noop,
}

/// Factory for creating bootstrap providers from configuration
///
/// This factory is designed to work with plugin-based providers loaded from
/// separate crates. The default behavior returns errors for non-noop providers
/// to encourage use of the dedicated plugin crates.
pub struct BootstrapProviderFactory;

impl BootstrapProviderFactory {
    /// Create a bootstrap provider from configuration
    ///
    /// Currently only supports the noop provider. Other providers are implemented
    /// in dedicated plugin crates and should be instantiated using the builder pattern:
    ///
    /// - PostgreSQL: Use `drasi_bootstrap_postgres::PostgresBootstrapProvider::builder().with_host(...).build()`
    /// - Platform: Use `drasi_bootstrap_platform::PlatformBootstrapProvider::builder().with_query_api_url(...).build()`
    /// - ScriptFile: Use `drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider::builder().with_file(...).build()`
    /// - Application: Use `drasi_bootstrap_application::ApplicationBootstrapProvider::builder().build()`
    pub fn create_provider(config: &BootstrapProviderConfig) -> Result<Box<dyn BootstrapProvider>> {
        match config {
            BootstrapProviderConfig::Postgres(_) => {
                Err(anyhow::anyhow!(
                    "PostgreSQL bootstrap provider is available in the drasi-bootstrap-postgres crate. \
                     Use PostgresBootstrapProvider::builder().with_host(...).build() to create it."
                ))
            }
            BootstrapProviderConfig::Application(_) => {
                Err(anyhow::anyhow!(
                    "Application bootstrap provider is available in the drasi-bootstrap-application crate. \
                     Use ApplicationBootstrapProvider::builder().build() to create it."
                ))
            }
            BootstrapProviderConfig::ScriptFile(config) => {
                Err(anyhow::anyhow!(
                    "ScriptFile bootstrap provider is available in the drasi-bootstrap-scriptfile crate. \
                     Use ScriptFileBootstrapProvider::builder().with_file(...).build() to create it. \
                     File paths: {:?}",
                    config.file_paths
                ))
            }
            BootstrapProviderConfig::Platform(config) => {
                Err(anyhow::anyhow!(
                    "Platform bootstrap provider is available in the drasi-bootstrap-platform crate. \
                     Use PlatformBootstrapProvider::builder().with_query_api_url(...).build() to create it. \
                     Config: {:?}",
                    config
                ))
            }
            BootstrapProviderConfig::Noop => {
                Err(anyhow::anyhow!(
                    "No-op bootstrap provider is available in the drasi-bootstrap-noop crate. \
                     Use NoOpBootstrapProvider::builder().build() or NoOpBootstrapProvider::new() to create it."
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_bootstrap_config_defaults() {
        let config = PlatformBootstrapConfig {
            query_api_url: Some("http://test:8080".to_string()), // DevSkim: ignore DS137138
            ..Default::default()
        };
        assert_eq!(config.timeout_seconds, 300);
        assert_eq!(config.query_api_url, Some("http://test:8080".to_string()));
    }

    #[test]
    fn test_postgres_bootstrap_config_defaults() {
        let config = PostgresBootstrapConfig::default();
        // Should be empty struct for now
        assert_eq!(config, PostgresBootstrapConfig {});
    }

    #[test]
    fn test_application_bootstrap_config_defaults() {
        let config = ApplicationBootstrapConfig::default();
        // Should be empty struct for now
        assert_eq!(config, ApplicationBootstrapConfig {});
    }

    #[test]
    fn test_platform_bootstrap_config_serialization() {
        let config = BootstrapProviderConfig::Platform(PlatformBootstrapConfig {
            query_api_url: Some("http://test:8080".to_string()),
            timeout_seconds: 600,
        });

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"type\":\"platform\""));
        assert!(json.contains("\"query_api_url\":\"http://test:8080\""));
        assert!(json.contains("\"timeout_seconds\":600"));

        let deserialized: BootstrapProviderConfig = serde_json::from_str(&json).unwrap();
        match deserialized {
            BootstrapProviderConfig::Platform(cfg) => {
                assert_eq!(cfg.query_api_url, Some("http://test:8080".to_string()));
                assert_eq!(cfg.timeout_seconds, 600);
            }
            _ => panic!("Expected Platform variant"),
        }
    }

    #[test]
    fn test_scriptfile_bootstrap_config() {
        let config = BootstrapProviderConfig::ScriptFile(ScriptFileBootstrapConfig {
            file_paths: vec![
                "/path/to/file1.jsonl".to_string(),
                "/path/to/file2.jsonl".to_string(),
            ],
        });

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"type\":\"scriptfile\""));
        assert!(json.contains("\"file_paths\""));

        let deserialized: BootstrapProviderConfig = serde_json::from_str(&json).unwrap();
        match deserialized {
            BootstrapProviderConfig::ScriptFile(cfg) => {
                assert_eq!(cfg.file_paths.len(), 2);
                assert_eq!(cfg.file_paths[0], "/path/to/file1.jsonl");
                assert_eq!(cfg.file_paths[1], "/path/to/file2.jsonl");
            }
            _ => panic!("Expected ScriptFile variant"),
        }
    }

    #[test]
    fn test_noop_bootstrap_config() {
        let config = BootstrapProviderConfig::Noop;

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"type\":\"noop\""));

        let deserialized: BootstrapProviderConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, BootstrapProviderConfig::Noop));
    }

    #[test]
    fn test_postgres_bootstrap_config_serialization() {
        let config = BootstrapProviderConfig::Postgres(PostgresBootstrapConfig::default());

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"type\":\"postgres\""));

        let deserialized: BootstrapProviderConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, BootstrapProviderConfig::Postgres(_)));
    }

    #[test]
    fn test_application_bootstrap_config_serialization() {
        let config = BootstrapProviderConfig::Application(ApplicationBootstrapConfig::default());

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"type\":\"application\""));

        let deserialized: BootstrapProviderConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized,
            BootstrapProviderConfig::Application(_)
        ));
    }

    #[test]
    fn test_yaml_deserialization_platform() {
        let yaml = r#"
type: platform
query_api_url: "http://remote:8080"
timeout_seconds: 300
"#;

        let config: BootstrapProviderConfig = serde_yaml::from_str(yaml).unwrap();
        match config {
            BootstrapProviderConfig::Platform(cfg) => {
                assert_eq!(cfg.query_api_url, Some("http://remote:8080".to_string()));
                assert_eq!(cfg.timeout_seconds, 300);
            }
            _ => panic!("Expected Platform variant"),
        }
    }

    #[test]
    fn test_yaml_deserialization_scriptfile() {
        let yaml = r#"
type: scriptfile
file_paths:
  - "/data/file1.jsonl"
  - "/data/file2.jsonl"
"#;

        let config: BootstrapProviderConfig = serde_yaml::from_str(yaml).unwrap();
        match config {
            BootstrapProviderConfig::ScriptFile(cfg) => {
                assert_eq!(cfg.file_paths.len(), 2);
                assert_eq!(cfg.file_paths[0], "/data/file1.jsonl");
            }
            _ => panic!("Expected ScriptFile variant"),
        }
    }

    #[test]
    fn test_platform_config_with_defaults() {
        let yaml = r#"
type: platform
query_api_url: "http://test:8080"
"#;

        let config: BootstrapProviderConfig = serde_yaml::from_str(yaml).unwrap();
        match config {
            BootstrapProviderConfig::Platform(cfg) => {
                assert_eq!(cfg.timeout_seconds, 300); // Should use default
            }
            _ => panic!("Expected Platform variant"),
        }
    }

    #[test]
    fn test_bootstrap_config_equality() {
        let config1 = BootstrapProviderConfig::Platform(PlatformBootstrapConfig {
            query_api_url: Some("http://test:8080".to_string()),
            timeout_seconds: 300,
        });

        let config2 = BootstrapProviderConfig::Platform(PlatformBootstrapConfig {
            query_api_url: Some("http://test:8080".to_string()),
            timeout_seconds: 300,
        });

        assert_eq!(config1, config2);
    }

    #[test]
    fn test_backward_compatibility_yaml() {
        // This YAML format should still work
        let yaml = r#"
type: postgres
"#;

        let config: BootstrapProviderConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(config, BootstrapProviderConfig::Postgres(_)));
    }
}
