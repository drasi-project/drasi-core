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

//! Source configuration builders

use crate::api::Properties;
use crate::bootstrap::{self, BootstrapProviderConfig};
use crate::channels::DispatchMode;
use crate::config::{SourceConfig, SourceSpecificConfig};
use crate::config::typed::*;
use serde_json::Value;
use std::collections::HashMap;

/// Fluent builder for Source configuration
#[derive(Debug, Clone)]
pub struct SourceBuilder {
    id: String,
    source_type: String,
    auto_start: bool,
    properties: HashMap<String, Value>,
    bootstrap_provider: Option<BootstrapProviderConfig>,
    dispatch_buffer_capacity: Option<usize>,
    dispatch_mode: Option<DispatchMode>,
}

impl SourceBuilder {
    /// Create a new source builder
    fn new(id: impl Into<String>, source_type: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            source_type: source_type.into(),
            auto_start: true,
            properties: HashMap::new(),
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
        }
    }

    /// Set whether to auto-start this source (default: true)
    pub fn auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Add a string property
    pub fn with_property(mut self, key: impl Into<String>, value: Value) -> Self {
        self.properties.insert(key.into(), value);
        self
    }

    /// Set properties using the Properties builder
    pub fn with_properties(mut self, properties: Properties) -> Self {
        self.properties = properties.build();
        self
    }

    /// Set a bootstrap provider from configuration
    pub fn with_bootstrap(mut self, config: BootstrapProviderConfig) -> Self {
        self.bootstrap_provider = Some(config);
        self
    }

    /// Set a script file bootstrap provider
    pub fn with_script_bootstrap(mut self, file_paths: Vec<String>) -> Self {
        self.bootstrap_provider = Some(BootstrapProviderConfig::ScriptFile(
            bootstrap::ScriptFileBootstrapConfig { file_paths }
        ));
        self
    }

    /// Set a platform bootstrap provider
    pub fn with_platform_bootstrap(
        mut self,
        query_api_url: impl Into<String>,
        timeout_seconds: Option<u64>,
    ) -> Self {
        self.bootstrap_provider = Some(BootstrapProviderConfig::Platform(
            bootstrap::PlatformBootstrapConfig {
                query_api_url: Some(query_api_url.into()),
                timeout_seconds: timeout_seconds.unwrap_or(300),
            }
        ));
        self
    }

    /// Set a PostgreSQL bootstrap provider
    pub fn with_postgres_bootstrap(mut self) -> Self {
        self.bootstrap_provider = Some(BootstrapProviderConfig::Postgres(
            bootstrap::PostgresBootstrapConfig::default()
        ));
        self
    }

    /// Set the dispatch buffer capacity for this source
    ///
    /// This overrides the global default dispatch buffer capacity.
    /// Controls the channel capacity for event routing to queries.
    ///
    /// Default: Inherits from server global setting (or 1000 if not specified)
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the dispatch mode for this source
    ///
    /// - `DispatchMode::Broadcast`: Single channel shared by all subscribers (memory efficient)
    /// - `DispatchMode::Channel`: Separate channel per subscriber (default, better isolation)
    ///
    /// Default: Channel mode
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Build the source configuration
    pub fn build(self) -> SourceConfig {
        // Convert properties HashMap to typed config based on source_type
        let config = self.build_typed_config();

        SourceConfig {
            id: self.id,
            auto_start: self.auto_start,
            config,
            bootstrap_provider: self.bootstrap_provider,
            dispatch_buffer_capacity: self.dispatch_buffer_capacity,
            dispatch_mode: self.dispatch_mode,
        }
    }

    /// Helper to build typed config from properties
    fn build_typed_config(&self) -> SourceSpecificConfig {
        match self.source_type.as_str() {
            "mock" => {
                let data_type = self.properties
                    .get("data_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("generic")
                    .to_string();
                let interval_ms = self.properties
                    .get("interval_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5000);
                SourceSpecificConfig::Mock(MockSourceConfig {
                    data_type,
                    interval_ms,
                })
            }
            "postgres" => {
                let host = self.properties
                    .get("host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("localhost")
                    .to_string();
                let port = self.properties
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5432) as u16;
                let database = self.properties
                    .get("database")
                    .and_then(|v| v.as_str())
                    .unwrap_or("postgres")
                    .to_string();
                let user = self.properties
                    .get("user")
                    .and_then(|v| v.as_str())
                    .unwrap_or("postgres")
                    .to_string();
                let password = self.properties
                    .get("password")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tables = self.properties
                    .get("tables")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let slot_name = self.properties
                    .get("slot_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("drasi_slot")
                    .to_string();
                let publication_name = self.properties
                    .get("publication_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("drasi_publication")
                    .to_string();
                let ssl_mode = self.properties
                    .get("ssl_mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("prefer")
                    .to_string();

                SourceSpecificConfig::Postgres(PostgresSourceConfig {
                    host,
                    port,
                    database,
                    user,
                    password,
                    tables,
                    slot_name,
                    publication_name,
                    ssl_mode,
                    table_keys: Vec::new(),
                })
            }
            "http" => {
                let host = self.properties
                    .get("host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("localhost")
                    .to_string();
                let port = self.properties
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(8080) as u16;
                let endpoint = self.properties
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let timeout_ms = self.properties
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30000);
                let database = self.properties
                    .get("database")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let user = self.properties
                    .get("user")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let password = self.properties
                    .get("password")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let adaptive_enabled = self.properties
                    .get("adaptive_enabled")
                    .and_then(|v| v.as_bool());
                let adaptive_max_batch_size = self.properties
                    .get("adaptive_max_batch_size")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                let adaptive_min_batch_size = self.properties
                    .get("adaptive_min_batch_size")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                let adaptive_max_wait_ms = self.properties
                    .get("adaptive_max_wait_ms")
                    .and_then(|v| v.as_u64());
                let adaptive_min_wait_ms = self.properties
                    .get("adaptive_min_wait_ms")
                    .and_then(|v| v.as_u64());
                let adaptive_window_secs = self.properties
                    .get("adaptive_window_secs")
                    .and_then(|v| v.as_u64());

                SourceSpecificConfig::Http(HttpSourceConfig {
                    host,
                    port,
                    endpoint,
                    timeout_ms,
                    tables: Vec::new(),
                    table_keys: Vec::new(),
                    database,
                    user,
                    password,
                    adaptive_enabled,
                    adaptive_max_batch_size,
                    adaptive_min_batch_size,
                    adaptive_max_wait_ms,
                    adaptive_min_wait_ms,
                    adaptive_window_secs,
                })
            }
            "grpc" => {
                let host = self.properties
                    .get("host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("0.0.0.0")
                    .to_string();
                let port = self.properties
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50051) as u16;
                let endpoint = self.properties
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let timeout_ms = self.properties
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30000);

                SourceSpecificConfig::Grpc(GrpcSourceConfig {
                    host,
                    port,
                    endpoint,
                    timeout_ms,
                })
            }
            "platform" => {
                let redis_url = self.properties
                    .get("redis_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("redis://localhost:6379")
                    .to_string();
                let stream_key = self.properties
                    .get("stream_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("drasi:changes")
                    .to_string();
                let consumer_group = self.properties
                    .get("consumer_group")
                    .and_then(|v| v.as_str())
                    .unwrap_or("drasi-core")
                    .to_string();
                let consumer_name = self.properties
                    .get("consumer_name")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let batch_size = self.properties
                    .get("batch_size")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10) as usize;
                let block_ms = self.properties
                    .get("block_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5000);

                SourceSpecificConfig::Platform(PlatformSourceConfig {
                    redis_url,
                    stream_key,
                    consumer_group,
                    consumer_name,
                    batch_size,
                    block_ms,
                })
            }
            "application" => {
                SourceSpecificConfig::Application(ApplicationSourceConfig {
                    properties: self.properties.clone(),
                })
            }
            _ => {
                // Custom source type
                SourceSpecificConfig::Custom {
                    properties: self.properties.clone(),
                }
            }
        }
    }
}

/// Source configuration factory
pub struct Source;

impl Source {
    /// Create an application source (for programmatic event injection)
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::Source;
    ///
    /// let source = Source::application("my-source")
    ///     .auto_start(true)
    ///     .build();
    /// ```
    pub fn application(id: impl Into<String>) -> SourceBuilder {
        SourceBuilder::new(id, "application")
    }

    /// Create a PostgreSQL source
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::{Source, Properties};
    /// use serde_json::json;
    ///
    /// let source = Source::postgres("pg-source")
    ///     .with_properties(
    ///         Properties::new()
    ///             .with_string("host", "localhost")
    ///             .with_string("database", "mydb")
    ///             .with_string("user", "postgres")
    ///             .with_string("password", "secret")
    ///     )
    ///     .with_postgres_bootstrap()
    ///     .build();
    /// ```
    pub fn postgres(id: impl Into<String>) -> SourceBuilder {
        SourceBuilder::new(id, "postgres")
    }

    /// Create a mock source (for testing)
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::{Source, Properties};
    /// use serde_json::json;
    ///
    /// let source = Source::mock("mock-source")
    ///     .with_property("interval_ms", json!(1000))
    ///     .build();
    /// ```
    pub fn mock(id: impl Into<String>) -> SourceBuilder {
        SourceBuilder::new(id, "mock")
    }

    /// Create a platform source (Redis Streams)
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::{Source, Properties};
    ///
    /// let source = Source::platform("platform-source")
    ///     .with_properties(
    ///         Properties::new()
    ///             .with_string("redis_url", "redis://localhost:6379")
    ///             .with_string("stream_key", "my-stream:changes")
    ///             .with_string("consumer_group", "drasi-core")
    ///             .with_string("consumer_name", "consumer-1")
    ///     )
    ///     .build();
    /// ```
    pub fn platform(id: impl Into<String>) -> SourceBuilder {
        SourceBuilder::new(id, "platform")
    }

    /// Create a gRPC source
    pub fn grpc(id: impl Into<String>) -> SourceBuilder {
        SourceBuilder::new(id, "grpc")
    }

    /// Create an HTTP source
    pub fn http(id: impl Into<String>) -> SourceBuilder {
        SourceBuilder::new(id, "http")
    }

    /// Create a custom source with specified type
    pub fn custom(id: impl Into<String>, source_type: impl Into<String>) -> SourceBuilder {
        SourceBuilder::new(id, source_type)
    }
}
