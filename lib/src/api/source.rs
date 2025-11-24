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
use crate::sources::application::ApplicationSourceConfig;
use crate::sources::grpc::GrpcSourceConfig;
use crate::sources::http::HttpSourceConfig;
use crate::sources::mock::MockSourceConfig;
use crate::sources::platform::PlatformSourceConfig;
use crate::sources::postgres::PostgresSourceConfig;
use serde_json::Value;
use std::collections::HashMap;

/// Fluent builder for Source configuration
///
/// `SourceBuilder` provides a type-safe, ergonomic API for configuring data sources in Drasi.
/// Sources ingest data changes from external systems and feed them to continuous queries.
///
/// # Builder Pattern
///
/// The builder uses a fluent API where each method returns `self`, allowing you to chain
/// configuration calls. Call `build()` at the end to create the [`SourceConfig`].
///
/// # Source Types
///
/// Drasi supports multiple source types, each created via factory methods on [`Source`]:
///
/// - **Application**: Programmatic event injection via API
/// - **PostgreSQL**: Change Data Capture from PostgreSQL WAL
/// - **HTTP**: HTTP endpoint that receives change events
/// - **gRPC**: gRPC service for receiving change events
/// - **Platform**: Redis Streams integration with external Drasi sources
/// - **Mock**: Testing source that generates synthetic data
///
/// # Bootstrap Providers
///
/// All sources support pluggable bootstrap providers for initial data delivery:
///
/// - **ScriptFile**: Load initial data from JSONL files (best for testing/development)
/// - **PostgreSQL**: Snapshot-based bootstrap from PostgreSQL database
/// - **Platform**: Bootstrap from remote Drasi Query API service
/// - **Application**: Replay stored insert events (internal only)
/// - **None**: No initial data (default for most sources)
///
/// Bootstrap providers are independent from sources - you can use any provider with any source.
/// For example, bootstrap from PostgreSQL but stream changes via HTTP.
///
/// # Performance Settings
///
/// - **dispatch_buffer_capacity**: Channel buffer size for event routing (default: 1000)
/// - **dispatch_mode**: Broadcast (shared channel) or Channel (per-subscriber)
///
/// # Default Values
///
/// - **auto_start**: true (source starts when server starts)
/// - **dispatch_mode**: Channel (separate channel per query)
/// - **bootstrap_provider**: None (no initial data)
///
/// # Thread Safety
///
/// `SourceBuilder` is `Clone` and can be used to create multiple source configurations.
/// The built configuration is immutable and thread-safe.
///
/// # Examples
///
/// ## Application Source (Programmatic)
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(
///         Source::application("events")
///             .auto_start(true)
///             .with_dispatch_buffer_capacity(5000)
///             .build()
///     )
///     .build()
///     .await?;
///
/// // Get handle and send events
/// let handle = core.source_handle("events").await?;
/// # Ok(())
/// # }
/// ```
///
/// ## PostgreSQL Source with Replication
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Properties};
/// # use serde_json::json;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(
///         Source::postgres("orders_db")
///             .with_properties(
///                 Properties::new()
///                     .with_string("host", "localhost")
///                     .with_int("port", 5432)
///                     .with_string("database", "orders")
///                     .with_string("user", "postgres")
///                     .with_string("password", "secret")
///                     .with_value("tables", json!(["orders", "customers"]))
///                     .with_string("slot_name", "drasi_slot")
///             )
///             .with_postgres_bootstrap()  // Snapshot before streaming
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## HTTP Source with ScriptFile Bootstrap
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Properties};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Load test data from file, then stream changes via HTTP
/// let core = DrasiServerCore::builder()
///     .add_source(
///         Source::http("events")
///             .with_properties(
///                 Properties::new()
///                     .with_string("host", "0.0.0.0")
///                     .with_int("port", 8080)
///                     .with_string("endpoint", "/changes")
///             )
///             .with_script_bootstrap(vec![
///                 "/data/initial_nodes.jsonl".to_string(),
///                 "/data/initial_relations.jsonl".to_string(),
///             ])
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Platform Source (Redis Streams)
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Properties};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(
///         Source::platform("external_source")
///             .with_properties(
///                 Properties::new()
///                     .with_string("redis_url", "redis://localhost:6379")
///                     .with_string("stream_key", "sensor-data:changes")
///                     .with_string("consumer_group", "drasi-core")
///             )
///             .with_platform_bootstrap("http://remote-drasi:8080", None)
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Mix-and-Match: Any Source with Any Bootstrap
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Properties};
/// # use serde_json::json;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Bootstrap 1M records from PostgreSQL, stream changes via HTTP
/// let core = DrasiServerCore::builder()
///     .add_source(
///         Source::http("high_volume")
///             .with_properties(
///                 Properties::new()
///                     .with_string("host", "0.0.0.0")
///                     .with_int("port", 9000)
///                     // PostgreSQL bootstrap properties
///                     .with_string("database", "mydb")
///                     .with_string("user", "dbuser")
///                     .with_string("password", "dbpass")
///                     .with_value("tables", json!(["stocks"]))
///             )
///             .with_postgres_bootstrap()  // Initial data from PostgreSQL
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
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

    /// Set whether to auto-start this source when the server starts
    ///
    /// When `auto_start` is `true` (the default), the source will automatically begin processing
    /// when [`DrasiServerCore::start()`](crate::DrasiServerCore::start) is called. When `false`,
    /// the source must be started manually via the control API.
    ///
    /// # Arguments
    ///
    /// * `auto_start` - Whether to automatically start this source (default: true)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Source that requires manual start (e.g., after external setup)
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::application("events")
    ///             .auto_start(false)  // Don't start automatically
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    ///
    /// // Start manually when ready
    /// // control_api.start_source("events").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Add a single property as a raw JSON value
    ///
    /// This method allows setting properties using raw `serde_json::Value` types. For a more
    /// ergonomic API with type safety, use [`with_properties()`](Self::with_properties) with
    /// the [`Properties`] builder instead.
    ///
    /// # Arguments
    ///
    /// * `key` - Property name
    /// * `value` - Property value as `serde_json::Value`
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source};
    /// # use serde_json::json;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::postgres("db")
    ///             .with_property("host", json!("localhost"))
    ///             .with_property("port", json!(5432))
    ///             .with_property("database", json!("mydb"))
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_property(mut self, key: impl Into<String>, value: Value) -> Self {
        self.properties.insert(key.into(), value);
        self
    }

    /// Set all properties using the Properties builder (recommended)
    ///
    /// This is the recommended way to configure source properties as it provides a type-safe,
    /// ergonomic API. Use [`Properties::new()`] to create a builder, then chain property setters,
    /// and pass the result to this method.
    ///
    /// This method replaces any previously set properties.
    ///
    /// # Arguments
    ///
    /// * `properties` - Properties builder instance
    ///
    /// # Examples
    ///
    /// ## PostgreSQL Source
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # use serde_json::json;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::postgres("orders_db")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "localhost")
    ///                     .with_int("port", 5432)
    ///                     .with_string("database", "orders")
    ///                     .with_string("user", "postgres")
    ///                     .with_string("password", "secret")
    ///                     .with_value("tables", json!(["orders", "customers"]))
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## HTTP Source
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::http("webhook")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "0.0.0.0")
    ///                     .with_int("port", 8080)
    ///                     .with_string("endpoint", "/events")
    ///                     .with_int("timeout_ms", 30000)
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_properties(mut self, properties: Properties) -> Self {
        self.properties = properties.build();
        self
    }

    /// Set a bootstrap provider from raw configuration
    ///
    /// This is a low-level method for setting bootstrap providers using raw configuration types.
    /// For most use cases, prefer the convenience methods:
    /// - [`with_script_bootstrap()`](Self::with_script_bootstrap) for JSONL files
    /// - [`with_postgres_bootstrap()`](Self::with_postgres_bootstrap) for PostgreSQL snapshot
    /// - [`with_platform_bootstrap()`](Self::with_platform_bootstrap) for remote Drasi Query API
    ///
    /// # Arguments
    ///
    /// * `config` - Raw bootstrap provider configuration
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source};
    /// # use drasi_lib::bootstrap::{BootstrapProviderConfig, ScriptFileBootstrapConfig};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let config = BootstrapProviderConfig::ScriptFile(
    ///     ScriptFileBootstrapConfig {
    ///         file_paths: vec!["/data/init.jsonl".to_string()],
    ///     }
    /// );
    ///
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::application("events")
    ///             .with_bootstrap(config)
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_bootstrap(mut self, config: BootstrapProviderConfig) -> Self {
        self.bootstrap_provider = Some(config);
        self
    }

    /// Set a script file bootstrap provider for loading initial data from JSONL files
    ///
    /// Script file bootstrap loads initial graph data from JSONL (JSON Lines) files before
    /// the source begins streaming changes. This is ideal for testing and development scenarios.
    ///
    /// # File Format
    ///
    /// Each line in the JSONL file must be a valid JSON object with a `type` field:
    /// - `Header`: Required first record (metadata)
    /// - `Node`: Insert a node with labels and properties
    /// - `Relation`: Insert a relationship between nodes
    /// - `Comment`: Ignored (for documentation)
    /// - `Label`: Checkpoint marker (for progress tracking)
    /// - `Finish`: Optional end marker
    ///
    /// # Arguments
    ///
    /// * `file_paths` - Vector of file paths to load in sequence
    ///
    /// # Examples
    ///
    /// ## Basic Usage
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::application("test_data")
    ///             .with_script_bootstrap(vec![
    ///                 "/data/nodes.jsonl".to_string(),
    ///                 "/data/relations.jsonl".to_string(),
    ///             ])
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Mix-and-Match: HTTP Source with File Bootstrap
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Load test data from file, stream changes via HTTP
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::http("events")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "0.0.0.0")
    ///                     .with_int("port", 8080)
    ///             )
    ///             .with_script_bootstrap(vec!["/data/initial.jsonl".to_string()])
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_script_bootstrap(mut self, file_paths: Vec<String>) -> Self {
        self.bootstrap_provider = Some(BootstrapProviderConfig::ScriptFile(
            bootstrap::ScriptFileBootstrapConfig { file_paths },
        ));
        self
    }

    /// Set a platform bootstrap provider to load initial data from a remote Drasi Query API
    ///
    /// Platform bootstrap fetches initial query results from a remote Drasi environment via
    /// HTTP streaming. This is useful for federating data across multiple Drasi instances or
    /// bootstrapping from a centralized data service.
    ///
    /// # Arguments
    ///
    /// * `query_api_url` - URL of the remote Drasi Query API (e.g., "http://remote-drasi:8080")
    /// * `timeout_seconds` - Optional timeout in seconds (default: 300)
    ///
    /// # Examples
    ///
    /// ## Basic Usage
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::platform("external_data")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("redis_url", "redis://localhost:6379")
    ///                     .with_string("stream_key", "sensor-data:changes")
    ///             )
    ///             .with_platform_bootstrap("http://remote-drasi:8080", Some(600))
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Mix-and-Match: gRPC Source with Platform Bootstrap
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Bootstrap from remote Drasi, stream changes via gRPC
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::grpc("events")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "0.0.0.0")
    ///                     .with_int("port", 50051)
    ///             )
    ///             .with_platform_bootstrap("http://central-drasi:8080", None)
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_platform_bootstrap(
        mut self,
        query_api_url: impl Into<String>,
        timeout_seconds: Option<u64>,
    ) -> Self {
        self.bootstrap_provider = Some(BootstrapProviderConfig::Platform(
            bootstrap::PlatformBootstrapConfig {
                query_api_url: Some(query_api_url.into()),
                timeout_seconds: timeout_seconds.unwrap_or(300),
            },
        ));
        self
    }

    /// Set a PostgreSQL bootstrap provider for snapshot-based initial data load
    ///
    /// PostgreSQL bootstrap takes a consistent snapshot of the database before beginning
    /// replication. This ensures the source starts with complete data and maintains consistency
    /// through LSN (Log Sequence Number) coordination.
    ///
    /// The bootstrap provider uses connection details from the source properties (host, database,
    /// user, password, tables, etc.). This method can be used with any source type, not just
    /// PostgreSQL sources.
    ///
    /// # Examples
    ///
    /// ## Standard PostgreSQL Source
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # use serde_json::json;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::postgres("orders_db")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "localhost")
    ///                     .with_int("port", 5432)
    ///                     .with_string("database", "orders")
    ///                     .with_string("user", "postgres")
    ///                     .with_string("password", "secret")
    ///                     .with_value("tables", json!(["orders", "customers"]))
    ///             )
    ///             .with_postgres_bootstrap()  // Snapshot before streaming
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Mix-and-Match: HTTP Source with PostgreSQL Bootstrap
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # use serde_json::json;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Bootstrap 1M records from PostgreSQL, stream changes via HTTP
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::http("high_volume")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "0.0.0.0")
    ///                     .with_int("port", 9000)
    ///                     // PostgreSQL bootstrap properties
    ///                     .with_string("database", "warehouse")
    ///                     .with_string("user", "dbuser")
    ///                     .with_string("password", "dbpass")
    ///                     .with_value("tables", json!(["inventory"]))
    ///             )
    ///             .with_postgres_bootstrap()  // Initial load from PostgreSQL
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_postgres_bootstrap(mut self) -> Self {
        self.bootstrap_provider = Some(BootstrapProviderConfig::Postgres(
            bootstrap::PostgresBootstrapConfig::default(),
        ));
        self
    }

    /// Set the dispatch buffer capacity for this source (performance tuning)
    ///
    /// Dispatch buffers are async channels used to route events from this source to subscribed
    /// queries. This setting overrides the global default for this specific source.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Maximum number of events to buffer (default: inherits from server, typically 1000)
    ///
    /// # Performance Implications
    ///
    /// - **Larger capacity**: Better burst handling, higher memory usage per query
    /// - **Smaller capacity**: Lower memory usage, backpressure during bursts
    ///
    /// # Examples
    ///
    /// ## High-Throughput Source
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // High-volume source with large buffer
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::postgres("high_volume_db")
    ///             .with_dispatch_buffer_capacity(50000)
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Low-Latency Source
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Low-latency source with small buffer
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::application("realtime_events")
    ///             .with_dispatch_buffer_capacity(100)
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the dispatch mode for this source (performance tuning)
    ///
    /// Dispatch mode controls how events are distributed from this source to subscribed queries:
    ///
    /// - **`DispatchMode::Channel`** (default): Separate channel per query
    ///   - Better isolation between queries
    ///   - Higher memory usage (N × buffer_capacity)
    ///   - Recommended for most use cases
    ///
    /// - **`DispatchMode::Broadcast`**: Single shared channel for all queries
    ///   - Memory efficient (1 × buffer_capacity)
    ///   - All queries must keep up or slow ones block fast ones
    ///   - Best when all queries have similar processing speeds
    ///
    /// # Arguments
    ///
    /// * `mode` - Dispatch mode (Channel or Broadcast)
    ///
    /// # Examples
    ///
    /// ## Broadcast Mode (Memory Efficient)
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, DispatchMode};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::application("events")
    ///             .with_dispatch_mode(DispatchMode::Broadcast)
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Channel Mode (Default, Better Isolation)
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, DispatchMode};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::postgres("db")
    ///             .with_dispatch_mode(DispatchMode::Channel)  // Explicit, but default
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Build the source configuration
    ///
    /// Consumes the builder and creates the final [`SourceConfig`] that can be passed to
    /// [`DrasiServerCoreBuilder::add_source()`](crate::DrasiServerCoreBuilder::add_source).
    ///
    /// # Returns
    ///
    /// Returns the configured [`SourceConfig`] ready for use.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let source_config = Source::application("events")
    ///     .auto_start(true)
    ///     .build();  // Build returns SourceConfig
    ///
    /// let core = DrasiServerCore::builder()
    ///     .add_source(source_config)
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
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
                let data_type = self
                    .properties
                    .get("data_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("generic")
                    .to_string();
                let interval_ms = self
                    .properties
                    .get("interval_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5000);
                SourceSpecificConfig::Mock(MockSourceConfig {
                    data_type,
                    interval_ms,
                })
            }
            "postgres" => {
                let host = self
                    .properties
                    .get("host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("localhost")
                    .to_string();
                let port = self
                    .properties
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5432) as u16;
                let database = self
                    .properties
                    .get("database")
                    .and_then(|v| v.as_str())
                    .unwrap_or("postgres")
                    .to_string();
                let user = self
                    .properties
                    .get("user")
                    .and_then(|v| v.as_str())
                    .unwrap_or("postgres")
                    .to_string();
                let password = self
                    .properties
                    .get("password")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tables = self
                    .properties
                    .get("tables")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let slot_name = self
                    .properties
                    .get("slot_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("drasi_slot")
                    .to_string();
                let publication_name = self
                    .properties
                    .get("publication_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("drasi_publication")
                    .to_string();
                let ssl_mode = self
                    .properties
                    .get("ssl_mode")
                    .and_then(|v| v.as_str())
                    .and_then(|s| match s {
                        "disable" => Some(crate::config::SslMode::Disable),
                        "prefer" => Some(crate::config::SslMode::Prefer),
                        "require" => Some(crate::config::SslMode::Require),
                        _ => None,
                    })
                    .unwrap_or(crate::config::SslMode::Prefer);

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
                let host = self
                    .properties
                    .get("host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("localhost")
                    .to_string();
                let port = self
                    .properties
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(8080) as u16;
                let endpoint = self
                    .properties
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let timeout_ms = self
                    .properties
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30000);
                // Note: database, user, password, tables, table_keys remain in properties map
                // for bootstrap providers to access via BootstrapContext
                let adaptive_enabled = self
                    .properties
                    .get("adaptive_enabled")
                    .and_then(|v| v.as_bool());
                let adaptive_max_batch_size = self
                    .properties
                    .get("adaptive_max_batch_size")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                let adaptive_min_batch_size = self
                    .properties
                    .get("adaptive_min_batch_size")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                let adaptive_max_wait_ms = self
                    .properties
                    .get("adaptive_max_wait_ms")
                    .and_then(|v| v.as_u64());
                let adaptive_min_wait_ms = self
                    .properties
                    .get("adaptive_min_wait_ms")
                    .and_then(|v| v.as_u64());
                let adaptive_window_secs = self
                    .properties
                    .get("adaptive_window_secs")
                    .and_then(|v| v.as_u64());

                SourceSpecificConfig::Http(HttpSourceConfig {
                    host,
                    port,
                    endpoint,
                    timeout_ms,
                    adaptive_enabled,
                    adaptive_max_batch_size,
                    adaptive_min_batch_size,
                    adaptive_max_wait_ms,
                    adaptive_min_wait_ms,
                    adaptive_window_secs,
                })
            }
            "grpc" => {
                let host = self
                    .properties
                    .get("host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("0.0.0.0")
                    .to_string();
                let port = self
                    .properties
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(50051) as u16;
                let endpoint = self
                    .properties
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let timeout_ms = self
                    .properties
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
                let redis_url = self
                    .properties
                    .get("redis_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("redis://localhost:6379")
                    .to_string();
                let stream_key = self
                    .properties
                    .get("stream_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("drasi:changes")
                    .to_string();
                let consumer_group = self
                    .properties
                    .get("consumer_group")
                    .and_then(|v| v.as_str())
                    .unwrap_or("drasi-core")
                    .to_string();
                let consumer_name = self
                    .properties
                    .get("consumer_name")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let batch_size = self
                    .properties
                    .get("batch_size")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10) as usize;
                let block_ms = self
                    .properties
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
            "application" => SourceSpecificConfig::Application(ApplicationSourceConfig {
                properties: self.properties.clone(),
            }),
            _ => {
                // Custom source type
                SourceSpecificConfig::Custom {
                    properties: self.properties.clone(),
                }
            }
        }
    }
}

/// Source configuration factory for creating sources with type-safe builders
///
/// `Source` is a zero-sized type that provides factory methods for creating different types
/// of sources. Each factory method returns a [`SourceBuilder`] configured for that source type.
///
/// # Source Types
///
/// - **Application**: Programmatic event injection via [`ApplicationSourceHandle`](crate::ApplicationSourceHandle)
/// - **PostgreSQL**: Change Data Capture from PostgreSQL WAL (Write-Ahead Log)
/// - **HTTP**: HTTP endpoint that receives change events via POST requests
/// - **gRPC**: gRPC service for receiving change events via streaming
/// - **Platform**: Redis Streams integration with external Drasi Platform sources
/// - **Mock**: Testing source that generates synthetic data at intervals
/// - **Custom**: User-defined source types for extensions
///
/// # Examples
///
/// ## Application Source
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(
///         Source::application("events")
///             .auto_start(true)
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## PostgreSQL Source with Bootstrap
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Properties};
/// # use serde_json::json;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(
///         Source::postgres("orders_db")
///             .with_properties(
///                 Properties::new()
///                     .with_string("host", "localhost")
///                     .with_int("port", 5432)
///                     .with_string("database", "orders")
///                     .with_string("user", "postgres")
///                     .with_string("password", "secret")
///                     .with_value("tables", json!(["orders", "customers"]))
///             )
///             .with_postgres_bootstrap()
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Multiple Sources
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Properties};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("app_events").build())
///     .add_source(
///         Source::postgres("db_changes")
///             .with_properties(
///                 Properties::new()
///                     .with_string("host", "localhost")
///                     .with_string("database", "mydb")
///             )
///             .build()
///     )
///     .add_source(
///         Source::http("webhook")
///             .with_properties(
///                 Properties::new()
///                     .with_string("host", "0.0.0.0")
///                     .with_int("port", 8080)
///             )
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct Source;

impl Source {
    /// Create an application source (for programmatic event injection)
    ///
    /// # Example
    /// ```no_run
    /// use drasi_lib::Source;
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
    /// use drasi_lib::{Source, Properties};
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
    /// use drasi_lib::{Source, Properties};
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
    /// use drasi_lib::{Source, Properties};
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

    /// Create a gRPC source for receiving change events via gRPC streaming
    ///
    /// gRPC sources receive change events through gRPC streaming RPC. Clients connect to the
    /// gRPC service and send events using the Drasi protocol buffer definitions.
    ///
    /// # Required Properties
    ///
    /// - `host`: Hostname or IP to bind to (e.g., "0.0.0.0", "localhost")
    /// - `port`: Port number to listen on (e.g., 50051)
    ///
    /// # Optional Properties
    ///
    /// - `timeout_ms`: Request timeout in milliseconds (default: 30000)
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source
    ///
    /// # Examples
    ///
    /// ## Basic gRPC Source
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::grpc("grpc_events")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "0.0.0.0")
    ///                     .with_int("port", 50051)
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## gRPC with Platform Bootstrap
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Bootstrap from remote Drasi, stream changes via gRPC
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::grpc("federated_data")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "0.0.0.0")
    ///                     .with_int("port", 50051)
    ///                     .with_int("timeout_ms", 60000)
    ///             )
    ///             .with_platform_bootstrap("http://central-drasi:8080", Some(600))
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn grpc(id: impl Into<String>) -> SourceBuilder {
        SourceBuilder::new(id, "grpc")
    }

    /// Create an HTTP source for receiving change events via HTTP POST
    ///
    /// HTTP sources expose an HTTP endpoint that receives change events via POST requests.
    /// The source supports adaptive mode for handling bursts and optional PostgreSQL properties
    /// for bootstrap integration.
    ///
    /// # Required Properties
    ///
    /// - `host`: Hostname or IP to bind to (e.g., "0.0.0.0", "localhost")
    /// - `port`: Port number to listen on (e.g., 8080)
    ///
    /// # Optional Properties
    ///
    /// - `endpoint`: HTTP path for receiving events (default: depends on implementation)
    /// - `timeout_ms`: Request timeout in milliseconds (default: 30000)
    /// - `adaptive_enabled`: Enable adaptive batching for bursts (default: false)
    /// - `adaptive_max_batch_size`: Maximum batch size in adaptive mode (default: 1000)
    /// - `database`, `user`, `password`, `tables`: PostgreSQL properties for bootstrap (when using `with_postgres_bootstrap()`)
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source
    ///
    /// # Examples
    ///
    /// ## Basic HTTP Source
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::http("webhook")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "0.0.0.0")
    ///                     .with_int("port", 8080)
    ///                     .with_string("endpoint", "/events")
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## HTTP with Adaptive Mode (High Throughput)
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::http("high_volume")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "0.0.0.0")
    ///                     .with_int("port", 9000)
    ///                     .with_bool("adaptive_enabled", true)
    ///                     .with_int("adaptive_max_batch_size", 5000)
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## HTTP with PostgreSQL Bootstrap (Mix-and-Match)
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # use serde_json::json;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Bootstrap 1M records from PostgreSQL, stream changes via HTTP
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::http("orders")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "0.0.0.0")
    ///                     .with_int("port", 8080)
    ///                     // PostgreSQL bootstrap properties
    ///                     .with_string("database", "orders_db")
    ///                     .with_string("user", "postgres")
    ///                     .with_string("password", "secret")
    ///                     .with_value("tables", json!(["orders", "customers"]))
    ///             )
    ///             .with_postgres_bootstrap()
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn http(id: impl Into<String>) -> SourceBuilder {
        SourceBuilder::new(id, "http")
    }

    /// Create a custom source with a user-defined type
    ///
    /// Custom sources allow you to define your own source types and implementations.
    /// This is useful for integrating with source plugins or extensions that are not
    /// part of the core Drasi source types.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source
    /// * `source_type` - Custom source type identifier (used to select the implementation)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Custom source type (requires corresponding implementation to be registered)
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::custom("kafka_events", "kafka")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("bootstrap_servers", "localhost:9092")
    ///                     .with_string("topic", "events")
    ///                     .with_string("group_id", "drasi-consumer")
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn custom(id: impl Into<String>, source_type: impl Into<String>) -> SourceBuilder {
        SourceBuilder::new(id, source_type)
    }
}
