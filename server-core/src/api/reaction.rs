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

//! Reaction configuration builders

use crate::api::Properties;
use crate::config::typed::*;
use crate::config::{ReactionConfig, ReactionSpecificConfig};
use serde_json::Value;
use std::collections::HashMap;

/// Fluent builder for Reaction configuration
///
/// `ReactionBuilder` provides a type-safe, ergonomic API for configuring reactions in Drasi.
/// Reactions subscribe to query results and deliver them to external systems or applications.
///
/// # Builder Pattern
///
/// The builder uses a fluent API where each method returns `self`, allowing you to chain
/// configuration calls. Call `build()` at the end to create the [`ReactionConfig`].
///
/// # Reaction Types
///
/// Drasi supports multiple reaction types, each created via factory methods on [`Reaction`]:
///
/// - **Application**: Programmatic result consumption via API
/// - **HTTP**: POST query results to HTTP webhooks
/// - **gRPC**: Stream results via gRPC
/// - **SSE**: Server-Sent Events for browser clients
/// - **Platform**: Redis Streams/Pub-Sub for Drasi Platform integration
/// - **Log**: Debug logging of results
/// - **Profiler**: Performance profiling and metrics
///
/// # Data Flow
///
/// ```text
/// Queries → [Priority Queue] → Reaction → External System
/// ```
///
/// 1. Reaction subscribes to one or more queries via `subscribe_to()` or `subscribe_to_queries()`
/// 2. Query results are buffered and timestamp-ordered in the priority queue
/// 3. Results are delivered to the reaction in temporal order
/// 4. Reaction forwards results to external system
///
/// # Performance Settings
///
/// - **priority_queue_capacity**: Buffer size for timestamp-ordered results (default: 10000)
///
/// # Default Values
///
/// - **auto_start**: true (reaction starts when server starts)
///
/// # Thread Safety
///
/// `ReactionBuilder` is `Clone` and can be used to create multiple reaction configurations.
/// The built configuration is immutable and thread-safe.
///
/// # Examples
///
/// ## Application Reaction (Programmatic)
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("events").build())
///     .add_query(
///         Query::cypher("active_users")
///             .query("MATCH (u:User) WHERE u.active = true RETURN u")
///             .from_source("events")
///             .build()
///     )
///     .add_reaction(
///         Reaction::application("app_reaction")
///             .subscribe_to("active_users")
///             .build()
///     )
///     .build()
///     .await?;
///
/// // Get handle and consume results
/// let handle = core.reaction_handle("app_reaction").await?;
/// # Ok(())
/// # }
/// ```
///
/// ## HTTP Webhook Reaction
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("orders").build())
///     .add_query(
///         Query::cypher("urgent_orders")
///             .query("MATCH (o:Order) WHERE o.priority = 'urgent' RETURN o")
///             .from_source("orders")
///             .build()
///     )
///     .add_reaction(
///         Reaction::http("webhook")
///             .subscribe_to("urgent_orders")
///             .with_properties(
///                 Properties::new()
///                     .with_string("base_url", "https://api.example.com/webhooks/orders")
///                     .with_string("token", "secret-token")
///                     .with_int("timeout_ms", 5000)
///             )
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Reaction Subscribing to Multiple Queries
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("events").build())
///     .add_query(Query::cypher("query1").query("MATCH (n:Event) RETURN n").from_source("events").build())
///     .add_query(Query::cypher("query2").query("MATCH (n:Alert) RETURN n").from_source("events").build())
///     .add_reaction(
///         Reaction::log("combined_logger")
///             .subscribe_to_queries(vec!["query1".to_string(), "query2".to_string()])
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## High-Reliability Reaction with Large Buffer
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::postgres("trading_db").build())
///     .add_query(
///         Query::cypher("stock_alerts")
///             .query("MATCH (s:Stock) WHERE s.change_percent > 10 RETURN s")
///             .from_source("trading_db")
///             .build()
///     )
///     .add_reaction(
///         Reaction::grpc("critical_alerts")
///             .subscribe_to("stock_alerts")
///             .with_priority_queue_capacity(500000)  // Large buffer for reliability
///             .with_properties(
///                 Properties::new()
///                     .with_string("endpoint", "grpc://alerts-service:50052")
///             )
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct ReactionBuilder {
    id: String,
    reaction_type: String,
    queries: Vec<String>,
    auto_start: bool,
    properties: HashMap<String, Value>,
    priority_queue_capacity: Option<usize>,
}

impl ReactionBuilder {
    /// Create a new reaction builder
    fn new(id: impl Into<String>, reaction_type: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            reaction_type: reaction_type.into(),
            queries: Vec::new(),
            auto_start: true,
            properties: HashMap::new(),
            priority_queue_capacity: None,
        }
    }

    /// Subscribe to a single query
    ///
    /// The reaction will receive all result changes from the specified query. Queries must be
    /// added to the server before reactions that subscribe to them.
    ///
    /// # Arguments
    ///
    /// * `query_id` - ID of the query to subscribe to
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(Query::cypher("my_query").query("MATCH (n) RETURN n").from_source("events").build())
    ///     .add_reaction(
    ///         Reaction::log("logger")
    ///             .subscribe_to("my_query")  // Subscribe to single query
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn subscribe_to(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Subscribe to multiple queries
    ///
    /// The reaction will receive result changes from all specified queries. This is useful for
    /// aggregating results from multiple queries into a single output destination.
    ///
    /// # Arguments
    ///
    /// * `query_ids` - Vector of query IDs to subscribe to
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(Query::cypher("high_priority").query("MATCH (e:Event) WHERE e.priority = 'high' RETURN e").from_source("events").build())
    ///     .add_query(Query::cypher("critical").query("MATCH (e:Event) WHERE e.priority = 'critical' RETURN e").from_source("events").build())
    ///     .add_reaction(
    ///         Reaction::http("webhook")
    ///             .subscribe_to_queries(vec!["high_priority".to_string(), "critical".to_string()])
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("base_url", "https://api.example.com/alerts")
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn subscribe_to_queries(mut self, query_ids: Vec<String>) -> Self {
        self.queries.extend(query_ids);
        self
    }

    /// Set whether to auto-start this reaction when the server starts
    ///
    /// When `auto_start` is `true` (the default), the reaction will automatically begin processing
    /// when [`DrasiServerCore::start()`](crate::DrasiServerCore::start) is called. When `false`,
    /// the reaction must be started manually via the control API.
    ///
    /// # Arguments
    ///
    /// * `auto_start` - Whether to automatically start this reaction (default: true)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Reaction that requires manual start (e.g., after external system is ready)
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(Query::cypher("my_query").query("MATCH (n) RETURN n").from_source("events").build())
    ///     .add_reaction(
    ///         Reaction::log("logger")
    ///             .subscribe_to("my_query")
    ///             .auto_start(false)  // Don't start automatically
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    ///
    /// // Start manually when ready
    /// // control_api.start_reaction("logger").await?;
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
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
    /// # use serde_json::json;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_query(Query::cypher("q").query("MATCH (n) RETURN n").from_source("s").build())
    ///     .add_reaction(
    ///         Reaction::http("webhook")
    ///             .subscribe_to("q")
    ///             .with_property("base_url", json!("https://api.example.com"))
    ///             .with_property("timeout_ms", json!(5000))
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
    /// This is the recommended way to configure reaction properties as it provides a type-safe,
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
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_query(Query::cypher("q").query("MATCH (n) RETURN n").from_source("s").build())
    ///     .add_reaction(
    ///         Reaction::http("webhook")
    ///             .subscribe_to("q")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("base_url", "https://api.example.com/webhook")
    ///                     .with_string("token", "secret-token")
    ///                     .with_int("timeout_ms", 10000)
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

    /// Set the priority queue capacity for this reaction (performance tuning)
    ///
    /// Priority queues buffer query results and order them by timestamp before delivering to
    /// the reaction. This ensures the reaction receives events in the correct temporal order
    /// even if queries emit events out of order.
    ///
    /// This setting overrides the global default for this specific reaction.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Maximum number of results to buffer (default: inherits from server, typically 10000)
    ///
    /// # Performance Implications
    ///
    /// - **Larger capacity**: Handles more out-of-order events, uses more memory, higher reliability
    /// - **Smaller capacity**: Lower memory usage, may block or drop events if they arrive very out of order
    ///
    /// # Recommended Values
    ///
    /// - **Critical reactions**: 100000-1000000 (high reliability, must not lose events)
    /// - **Normal reactions**: 10000 (default, suitable for most use cases)
    /// - **Memory-constrained**: 1000-5000 (when memory is limited)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_query(Query::cypher("q").query("MATCH (n) RETURN n").from_source("s").build())
    ///     .add_reaction(
    ///         Reaction::grpc("critical_alerts")
    ///             .subscribe_to("q")
    ///             .with_priority_queue_capacity(500000)  // Large buffer for critical system
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("endpoint", "grpc://alerts:50052")
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Build the reaction configuration
    ///
    /// Consumes the builder and creates the final [`ReactionConfig`] that can be passed to
    /// [`DrasiServerCoreBuilder::add_reaction()`](crate::DrasiServerCoreBuilder::add_reaction).
    ///
    /// # Returns
    ///
    /// Returns the configured [`ReactionConfig`] ready for use.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let reaction_config = Reaction::log("logger")
    ///     .subscribe_to("my_query")
    ///     .build();  // Build returns ReactionConfig
    ///
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(Query::cypher("my_query").query("MATCH (n) RETURN n").from_source("events").build())
    ///     .add_reaction(reaction_config)
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn build(self) -> ReactionConfig {
        // Convert properties HashMap to typed config based on reaction_type
        let config = self.build_typed_config();

        ReactionConfig {
            id: self.id,
            queries: self.queries,
            auto_start: self.auto_start,
            config,
            priority_queue_capacity: self.priority_queue_capacity,
        }
    }

    /// Helper to build typed config from properties
    fn build_typed_config(&self) -> ReactionSpecificConfig {
        match self.reaction_type.as_str() {
            "log" => {
                let log_level = self
                    .properties
                    .get("log_level")
                    .and_then(|v| v.as_str())
                    .unwrap_or("info")
                    .to_string();
                ReactionSpecificConfig::Log(LogReactionConfig { log_level })
            }
            "http" => {
                let base_url = self
                    .properties
                    .get("base_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("http://localhost")
                    .to_string();
                let token = self
                    .properties
                    .get("token")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let timeout_ms = self
                    .properties
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10000);

                ReactionSpecificConfig::Http(HttpReactionConfig {
                    base_url,
                    token,
                    timeout_ms,
                    queries: HashMap::new(),
                })
            }
            "grpc" => {
                let endpoint = self
                    .properties
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("grpc://localhost:50052")
                    .to_string();
                let token = self
                    .properties
                    .get("token")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let timeout_ms = self
                    .properties
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5000);
                let batch_size = self
                    .properties
                    .get("batch_size")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                let batch_flush_timeout_ms = self
                    .properties
                    .get("batch_flush_timeout_ms")
                    .and_then(|v| v.as_u64());
                let max_retries = self
                    .properties
                    .get("max_retries")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let connection_retry_attempts = self
                    .properties
                    .get("connection_retry_attempts")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let initial_connection_timeout_ms = self
                    .properties
                    .get("initial_connection_timeout_ms")
                    .and_then(|v| v.as_u64());

                ReactionSpecificConfig::Grpc(GrpcReactionConfig {
                    endpoint,
                    token,
                    timeout_ms,
                    batch_size: batch_size.unwrap_or(10),
                    batch_flush_timeout_ms: batch_flush_timeout_ms.unwrap_or(1000),
                    max_retries: max_retries.unwrap_or(3),
                    connection_retry_attempts: connection_retry_attempts.unwrap_or(5),
                    initial_connection_timeout_ms: initial_connection_timeout_ms.unwrap_or(10000),
                    metadata: HashMap::new(),
                })
            }
            "sse" => {
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
                let sse_path = self
                    .properties
                    .get("sse_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("/sse")
                    .to_string();
                let heartbeat_interval_ms = self
                    .properties
                    .get("heartbeat_interval_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30000);

                ReactionSpecificConfig::Sse(SseReactionConfig {
                    host,
                    port,
                    sse_path,
                    heartbeat_interval_ms,
                })
            }
            "platform" => {
                let redis_url = self
                    .properties
                    .get("redis_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("redis://localhost:6379")
                    .to_string();
                let pubsub_name = self
                    .properties
                    .get("pubsub_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("drasi")
                    .to_string();
                let source_name = self
                    .properties
                    .get("source_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("output")
                    .to_string();
                let max_stream_length = self
                    .properties
                    .get("max_stream_length")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1000) as usize;
                let emit_control_events = self
                    .properties
                    .get("emit_control_events")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let batch_enabled = self
                    .properties
                    .get("batch_enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let batch_max_size = self
                    .properties
                    .get("batch_max_size")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                let batch_max_wait_ms = self
                    .properties
                    .get("batch_max_wait_ms")
                    .and_then(|v| v.as_u64());

                ReactionSpecificConfig::Platform(PlatformReactionConfig {
                    redis_url,
                    pubsub_name: Some(pubsub_name),
                    source_name: Some(source_name),
                    max_stream_length: Some(max_stream_length),
                    emit_control_events,
                    batch_enabled,
                    batch_max_size: batch_max_size.unwrap_or(100),
                    batch_max_wait_ms: batch_max_wait_ms.unwrap_or(100),
                })
            }
            "profiler" => {
                let output_file = self
                    .properties
                    .get("output_file")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let detailed = self
                    .properties
                    .get("detailed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let window_size = self
                    .properties
                    .get("window_size")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1000) as usize;
                let report_interval_secs = self
                    .properties
                    .get("report_interval_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(60);

                ReactionSpecificConfig::Profiler(ProfilerReactionConfig {
                    output_file,
                    detailed,
                    window_size,
                    report_interval_secs,
                })
            }
            "application" => ReactionSpecificConfig::Application(ApplicationReactionConfig {
                properties: self.properties.clone(),
            }),
            _ => {
                // Use custom reaction type for unknown types
                ReactionSpecificConfig::Custom {
                    properties: self.properties.clone(),
                }
            }
        }
    }
}

/// Reaction configuration factory for creating reactions with type-safe builders
///
/// `Reaction` is a zero-sized type that provides factory methods for creating different types
/// of reactions. Each factory method returns a [`ReactionBuilder`] configured for that reaction type.
///
/// # Reaction Types
///
/// - **Application**: Programmatic result consumption via [`ApplicationReactionHandle`](crate::ApplicationReactionHandle)
/// - **HTTP**: POST query results to HTTP webhooks
/// - **gRPC**: Stream results via gRPC to external services
/// - **SSE**: Server-Sent Events for browser clients and real-time dashboards
/// - **Platform**: Redis Streams/Pub-Sub for Drasi Platform integration
/// - **Log**: Debug logging of query results
/// - **Profiler**: Performance profiling and metrics collection
///
/// # Examples
///
/// ## Application Reaction
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("events").build())
///     .add_query(Query::cypher("my_query").query("MATCH (n) RETURN n").from_source("events").build())
///     .add_reaction(
///         Reaction::application("app_reaction")
///             .subscribe_to("my_query")
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## HTTP Webhook Reaction
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("orders").build())
///     .add_query(Query::cypher("q").query("MATCH (o:Order) RETURN o").from_source("orders").build())
///     .add_reaction(
///         Reaction::http("webhook")
///             .subscribe_to("q")
///             .with_properties(
///                 Properties::new()
///                     .with_string("base_url", "https://api.example.com/webhook")
///                     .with_string("token", "secret-token")
///             )
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Multiple Reactions on Same Query
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("events").build())
///     .add_query(Query::cypher("alerts").query("MATCH (a:Alert) RETURN a").from_source("events").build())
///     .add_reaction(Reaction::log("logger").subscribe_to("alerts").build())
///     .add_reaction(
///         Reaction::http("webhook")
///             .subscribe_to("alerts")
///             .with_properties(Properties::new().with_string("base_url", "https://api.example.com"))
///             .build()
///     )
///     .add_reaction(Reaction::sse("dashboard").subscribe_to("alerts").build())
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct Reaction;

impl Reaction {
    /// Create an application reaction (for programmatic result consumption)
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::Reaction;
    ///
    /// let reaction = Reaction::application("my-reaction")
    ///     .subscribe_to("my-query")
    ///     .build();
    /// ```
    pub fn application(id: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, "application")
    }

    /// Create an HTTP reaction
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::{Reaction, Properties};
    ///
    /// let reaction = Reaction::http("http-reaction")
    ///     .subscribe_to("my-query")
    ///     .with_properties(
    ///         Properties::new()
    ///             .with_string("url", "http://localhost:8080/webhook")
    ///             .with_string("method", "POST")
    ///     )
    ///     .build();
    /// ```
    pub fn http(id: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, "http")
    }

    /// Create a gRPC reaction for streaming results via gRPC
    ///
    /// gRPC reactions stream query results to external gRPC services using the Drasi protocol
    /// buffer definitions. Supports batching, retries, and automatic reconnection.
    ///
    /// # Required Properties
    ///
    /// - `endpoint`: gRPC endpoint URL (e.g., "grpc://service:50052")
    ///
    /// # Optional Properties
    ///
    /// - `token`: Authentication token for the gRPC service
    /// - `timeout_ms`: Request timeout in milliseconds (default: 5000)
    /// - `batch_size`: Number of results to batch before sending (default: 10)
    /// - `batch_flush_timeout_ms`: Max time to wait before flushing batch (default: 1000)
    /// - `max_retries`: Maximum retry attempts per request (default: 3)
    /// - `connection_retry_attempts`: Connection retry attempts (default: 5)
    /// - `initial_connection_timeout_ms`: Initial connection timeout (default: 10000)
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this reaction
    ///
    /// # Examples
    ///
    /// ## Basic gRPC Reaction
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(Query::cypher("q").query("MATCH (n) RETURN n").from_source("events").build())
    ///     .add_reaction(
    ///         Reaction::grpc("grpc_output")
    ///             .subscribe_to("q")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("endpoint", "grpc://external-service:50052")
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## gRPC with Batching and Retries
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_query(Query::cypher("q").query("MATCH (n) RETURN n").from_source("s").build())
    ///     .add_reaction(
    ///         Reaction::grpc("high_throughput")
    ///             .subscribe_to("q")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("endpoint", "grpc://analytics:50052")
    ///                     .with_string("token", "secret-token")
    ///                     .with_int("batch_size", 100)
    ///                     .with_int("batch_flush_timeout_ms", 500)
    ///                     .with_int("max_retries", 5)
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn grpc(id: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, "grpc")
    }

    /// Create an SSE (Server-Sent Events) reaction for real-time browser clients
    ///
    /// SSE reactions expose an HTTP endpoint that streams query results to connected clients
    /// using Server-Sent Events. Perfect for real-time dashboards, live notifications, and
    /// browser-based applications.
    ///
    /// # Required Properties
    ///
    /// - `host`: Hostname or IP to bind to (e.g., "0.0.0.0", "localhost")
    /// - `port`: Port number to listen on (e.g., 8080)
    ///
    /// # Optional Properties
    ///
    /// - `sse_path`: HTTP path for SSE endpoint (default: "/sse")
    /// - `heartbeat_interval_ms`: Heartbeat interval to keep connections alive (default: 30000)
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this reaction
    ///
    /// # Examples
    ///
    /// ## Basic SSE Reaction for Dashboard
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("metrics").build())
    ///     .add_query(
    ///         Query::cypher("live_metrics")
    ///             .query("MATCH (m:Metric) RETURN m.name, m.value")
    ///             .from_source("metrics")
    ///             .build()
    ///     )
    ///     .add_reaction(
    ///         Reaction::sse("dashboard")
    ///             .subscribe_to("live_metrics")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "0.0.0.0")
    ///                     .with_int("port", 8080)
    ///                     .with_string("sse_path", "/metrics-stream")
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    ///
    /// // Browser clients can connect to: http://localhost:8080/metrics-stream
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## SSE with Custom Heartbeat
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_query(Query::cypher("q").query("MATCH (n) RETURN n").from_source("s").build())
    ///     .add_reaction(
    ///         Reaction::sse("realtime")
    ///             .subscribe_to("q")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("host", "0.0.0.0")
    ///                     .with_int("port", 3000)
    ///                     .with_int("heartbeat_interval_ms", 10000)  // 10s heartbeat
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn sse(id: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, "sse")
    }

    /// Create a log reaction (for debugging)
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::Reaction;
    ///
    /// let reaction = Reaction::log("log-reaction")
    ///     .subscribe_to("my-query")
    ///     .build();
    /// ```
    pub fn log(id: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, "log")
    }

    /// Create a custom reaction with a user-defined type
    ///
    /// Custom reactions allow you to define your own reaction types and implementations.
    /// This is useful for integrating with reaction plugins or extensions that are not
    /// part of the core Drasi reaction types.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this reaction
    /// * `reaction_type` - Custom reaction type identifier (used to select the implementation)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Custom reaction type (requires corresponding implementation to be registered)
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(Query::cypher("q").query("MATCH (n) RETURN n").from_source("events").build())
    ///     .add_reaction(
    ///         Reaction::custom("kafka_output", "kafka")
    ///             .subscribe_to("q")
    ///             .with_properties(
    ///                 Properties::new()
    ///                     .with_string("bootstrap_servers", "localhost:9092")
    ///                     .with_string("topic", "drasi-results")
    ///             )
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn custom(id: impl Into<String>, reaction_type: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, reaction_type)
    }
}
