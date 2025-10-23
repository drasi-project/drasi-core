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
use crate::bootstrap::BootstrapProviderConfig;
use crate::config::SourceConfig;
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
        self.bootstrap_provider = Some(BootstrapProviderConfig::ScriptFile { file_paths });
        self
    }

    /// Set a platform bootstrap provider
    pub fn with_platform_bootstrap(
        mut self,
        query_api_url: impl Into<String>,
        timeout_seconds: Option<u64>,
    ) -> Self {
        self.bootstrap_provider = Some(BootstrapProviderConfig::Platform {
            query_api_url: Some(query_api_url.into()),
            timeout_seconds,
            config: HashMap::new(),
        });
        self
    }

    /// Set a PostgreSQL bootstrap provider
    pub fn with_postgres_bootstrap(mut self) -> Self {
        self.bootstrap_provider = Some(BootstrapProviderConfig::Postgres {
            config: HashMap::new(),
        });
        self
    }

    /// Build the source configuration
    pub fn build(self) -> SourceConfig {
        SourceConfig {
            id: self.id,
            source_type: self.source_type,
            auto_start: self.auto_start,
            properties: self.properties,
            bootstrap_provider: self.bootstrap_provider,
            broadcast_channel_capacity: None, // Default: inherit from server global setting
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
