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

//! Typed configuration structs for sources and reactions.
//!
//! This module provides type-safe configuration alternatives to property maps,
//! offering compile-time validation and better IDE support.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// =============================================================================
// Common Enums
// =============================================================================

/// SSL mode for PostgreSQL connections
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    /// Disable SSL encryption
    Disable,
    /// Prefer SSL but allow unencrypted connections
    Prefer,
    /// Require SSL encryption
    Require,
}

impl Default for SslMode {
    fn default() -> Self {
        Self::Prefer
    }
}

impl std::fmt::Display for SslMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disable => write!(f, "disable"),
            Self::Prefer => write!(f, "prefer"),
            Self::Require => write!(f, "require"),
        }
    }
}

/// Log level for log reactions
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Trace level logging
    Trace,
    /// Debug level logging
    Debug,
    /// Info level logging
    Info,
    /// Warning level logging
    Warn,
    /// Error level logging
    Error,
}

impl Default for LogLevel {
    fn default() -> Self {
        Self::Info
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trace => write!(f, "trace"),
            Self::Debug => write!(f, "debug"),
            Self::Info => write!(f, "info"),
            Self::Warn => write!(f, "warn"),
            Self::Error => write!(f, "error"),
        }
    }
}

// =============================================================================
// Source Configuration Types
// =============================================================================

/// Mock source configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MockSourceConfig {
    /// Type of data to generate: "counter", "sensor", or "generic"
    #[serde(default = "default_data_type")]
    pub data_type: String,

    /// Interval between data generation in milliseconds
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
}

fn default_data_type() -> String {
    "generic".to_string()
}

fn default_interval_ms() -> u64 {
    5000
}

impl Default for MockSourceConfig {
    fn default() -> Self {
        Self {
            data_type: default_data_type(),
            interval_ms: default_interval_ms(),
        }
    }
}

/// PostgreSQL replication source configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostgresSourceConfig {
    /// PostgreSQL host
    #[serde(default = "default_postgres_host")]
    pub host: String,

    /// PostgreSQL port
    #[serde(default = "default_postgres_port")]
    pub port: u16,

    /// Database name
    pub database: String,

    /// Database user
    pub user: String,

    /// Database password
    #[serde(default)]
    pub password: String,

    /// Tables to replicate
    #[serde(default)]
    pub tables: Vec<String>,

    /// Replication slot name
    #[serde(default = "default_slot_name")]
    pub slot_name: String,

    /// Publication name
    #[serde(default = "default_publication_name")]
    pub publication_name: String,

    /// SSL mode
    #[serde(default)]
    pub ssl_mode: SslMode,

    /// Table key configurations
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,
}

fn default_postgres_host() -> String {
    "localhost".to_string()
}

fn default_postgres_port() -> u16 {
    5432
}

fn default_slot_name() -> String {
    "drasi_slot".to_string()
}

fn default_publication_name() -> String {
    "drasi_publication".to_string()
}

/// Table key configuration for PostgreSQL
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

/// HTTP source configuration
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

    /// Tables to monitor (for adaptive HTTP)
    #[serde(default)]
    pub tables: Vec<String>,

    /// Table key configurations (for adaptive HTTP)
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,

    /// PostgreSQL database (for adaptive HTTP bootstrap)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,

    /// PostgreSQL user (for adaptive HTTP bootstrap)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// PostgreSQL password (for adaptive HTTP bootstrap)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,

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
}

fn default_timeout_ms() -> u64 {
    10000
}

/// gRPC source configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrpcSourceConfig {
    /// gRPC server host
    pub host: String,

    /// gRPC server port
    pub port: u16,

    /// Optional service endpoint
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

/// Platform source configuration (Redis Streams)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlatformSourceConfig {
    /// Redis connection URL
    pub redis_url: String,

    /// Redis stream key to consume from
    pub stream_key: String,

    /// Consumer group name
    #[serde(default = "default_consumer_group")]
    pub consumer_group: String,

    /// Consumer name (unique within group)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_name: Option<String>,

    /// Batch size for reading events
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// Block timeout in milliseconds
    #[serde(default = "default_block_ms")]
    pub block_ms: u64,
}

fn default_consumer_group() -> String {
    "drasi-core".to_string()
}

fn default_batch_size() -> usize {
    100
}

fn default_block_ms() -> u64 {
    5000
}

/// Application source configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplicationSourceConfig {
    /// Application-specific properties (for now, keep flexible)
    #[serde(flatten)]
    pub properties: HashMap<String, serde_json::Value>,
}

/// Enum wrapping all source-specific configurations
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "source_type", rename_all = "lowercase")]
pub enum SourceSpecificConfig {
    Mock(MockSourceConfig),
    Postgres(PostgresSourceConfig),
    Http(HttpSourceConfig),
    Grpc(GrpcSourceConfig),
    Platform(PlatformSourceConfig),
    Application(ApplicationSourceConfig),

    /// Custom source type with properties map (for extensibility)
    #[serde(rename = "custom")]
    Custom {
        #[serde(flatten)]
        properties: HashMap<String, serde_json::Value>,
    },
}

// =============================================================================
// Reaction Configuration Types
// =============================================================================

/// Log reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogReactionConfig {
    /// Log level
    #[serde(default)]
    pub log_level: LogLevel,
}

impl Default for LogReactionConfig {
    fn default() -> Self {
        Self {
            log_level: LogLevel::default(),
        }
    }
}

// Re-export CallSpec from the http reaction module
// Note: We don't re-export QueryConfig to avoid ambiguity with schema::QueryConfig
pub use crate::reactions::http::CallSpec;
// Import QueryConfig for use in reaction configs
use crate::reactions::http::QueryConfig as HttpQueryConfig;

/// HTTP reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpReactionConfig {
    /// Base URL for HTTP requests
    #[serde(default = "default_base_url")]
    pub base_url: String,

    /// Optional authentication token
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Query-specific call configurations
    #[serde(default)]
    pub routes: HashMap<String, HttpQueryConfig>,
}

fn default_base_url() -> String {
    "http://localhost".to_string()
}

// =============================================================================
// Adaptive Reaction Configuration Types
// =============================================================================

/// Adaptive batching configuration shared by adaptive reactions
///
/// This configuration is used by both HTTP Adaptive and gRPC Adaptive reactions to
/// dynamically adjust batch size and timing based on throughput patterns.
///
/// # Adaptive Algorithm
///
/// The adaptive batcher monitors throughput over a configurable window and adjusts
/// batch size and wait time based on traffic level:
///
/// | Throughput Level | Messages/Sec | Batch Size | Wait Time |
/// |-----------------|--------------|------------|-----------|
/// | Idle           | < 1          | min_batch_size | 1ms |
/// | Low            | 1-100        | 2 × min | 1ms |
/// | Medium         | 100-1K       | 25% of max | 10ms |
/// | High           | 1K-10K       | 50% of max | 25ms |
/// | Burst          | > 10K        | max_batch_size | 50ms |
///
/// # Example
///
/// ```rust
/// use drasi_server_core::config::typed::AdaptiveBatchConfig;
///
/// // High-throughput configuration
/// let config = AdaptiveBatchConfig {
///     adaptive_min_batch_size: 50,
///     adaptive_max_batch_size: 2000,
///     adaptive_window_size: 100,  // 10 seconds
///     adaptive_batch_timeout_ms: 500,
/// };
///
/// // Low-latency configuration
/// let config = AdaptiveBatchConfig {
///     adaptive_min_batch_size: 1,
///     adaptive_max_batch_size: 50,
///     adaptive_window_size: 30,  // 3 seconds
///     adaptive_batch_timeout_ms: 10,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdaptiveBatchConfig {
    /// Minimum batch size (events per batch) used during idle/low traffic
    #[serde(default = "default_adaptive_min_batch_size")]
    pub adaptive_min_batch_size: usize,

    /// Maximum batch size (events per batch) used during burst traffic
    #[serde(default = "default_adaptive_max_batch_size")]
    pub adaptive_max_batch_size: usize,

    /// Window size for throughput monitoring in 100ms units (range: 1-255)
    ///
    /// For example:
    /// - 10 = 1 second
    /// - 50 = 5 seconds
    /// - 100 = 10 seconds
    ///
    /// Larger windows provide more stable adaptation but respond slower to traffic changes.
    #[serde(default = "default_adaptive_window_size")]
    pub adaptive_window_size: usize,

    /// Maximum time to wait before flushing a partial batch (milliseconds)
    ///
    /// This timeout ensures results are delivered even when batch size is not reached.
    #[serde(default = "default_adaptive_batch_timeout_ms")]
    pub adaptive_batch_timeout_ms: u64,
}

impl Default for AdaptiveBatchConfig {
    fn default() -> Self {
        Self {
            adaptive_min_batch_size: default_adaptive_min_batch_size(),
            adaptive_max_batch_size: default_adaptive_max_batch_size(),
            adaptive_window_size: default_adaptive_window_size(),
            adaptive_batch_timeout_ms: default_adaptive_batch_timeout_ms(),
        }
    }
}

fn default_adaptive_min_batch_size() -> usize {
    1
}

fn default_adaptive_max_batch_size() -> usize {
    100
}

fn default_adaptive_window_size() -> usize {
    10
}

fn default_adaptive_batch_timeout_ms() -> u64 {
    1000
}

/// gRPC Adaptive reaction configuration with adaptive batching
///
/// This reaction extends the standard gRPC reaction with intelligent, throughput-based
/// batching that automatically adjusts batch sizes and wait times based on real-time
/// traffic patterns.
///
/// # Key Features
///
/// - **Dynamic Batching**: Adjusts batch size (min to max) based on throughput
/// - **Lazy Connection**: Connects only when first batch is ready
/// - **Automatic Retry**: Exponential backoff with configurable retries
/// - **Traffic Classification**: Five levels (Idle/Low/Medium/High/Burst)
///
/// # Example
///
/// ```rust
/// use drasi_server_core::config::{ReactionConfig, ReactionSpecificConfig};
/// use drasi_server_core::config::typed::{GrpcAdaptiveReactionConfig, AdaptiveBatchConfig};
/// use std::collections::HashMap;
///
/// let config = ReactionConfig {
///     id: "high-throughput".to_string(),
///     queries: vec!["event-stream".to_string()],
///     auto_start: true,
///     config: ReactionSpecificConfig::GrpcAdaptive(GrpcAdaptiveReactionConfig {
///         endpoint: "grpc://event-processor:9090".to_string(),
///         timeout_ms: 15000,
///         max_retries: 5,
///         connection_retry_attempts: 5,
///         initial_connection_timeout_ms: 10000,
///         metadata: HashMap::new(),
///         adaptive: AdaptiveBatchConfig {
///             adaptive_min_batch_size: 50,
///             adaptive_max_batch_size: 2000,
///             adaptive_window_size: 100,  // 10 seconds
///             adaptive_batch_timeout_ms: 500,
///         },
///     }),
///     priority_queue_capacity: None,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrpcAdaptiveReactionConfig {
    /// gRPC server endpoint URL (e.g., "grpc://localhost:50052")
    #[serde(default = "default_grpc_endpoint")]
    pub endpoint: String,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Maximum retries for failed batch requests
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Number of connection retry attempts before giving up
    #[serde(default = "default_connection_retry_attempts")]
    pub connection_retry_attempts: u32,

    /// Initial connection timeout in milliseconds (used for lazy connection)
    #[serde(default = "default_initial_connection_timeout_ms")]
    pub initial_connection_timeout_ms: u64,

    /// gRPC metadata headers to include in all requests
    #[serde(default)]
    pub metadata: HashMap<String, String>,

    /// Adaptive batching configuration (flattened into parent config)
    #[serde(flatten)]
    pub adaptive: AdaptiveBatchConfig,
}

/// HTTP Adaptive reaction configuration with adaptive batching
///
/// This reaction extends the standard HTTP reaction with intelligent batching and
/// HTTP/2 connection pooling, automatically adjusting batch size and timing based
/// on throughput patterns.
///
/// # Key Features
///
/// - **Intelligent Batching**: Groups multiple results based on traffic patterns
/// - **Batch Endpoint**: Sends batches to `{base_url}/batch` endpoint
/// - **Adaptive Algorithm**: Dynamically adjusts batch size and wait time
/// - **HTTP/2 Pooling**: Maintains persistent connections for better performance
/// - **Individual Fallback**: Uses query-specific routes for single results
///
/// # Batch Endpoint Format
///
/// Batches are sent as POST requests to `{base_url}/batch` with an array of
/// `BatchResult` objects containing query_id, results array, timestamp, and count.
///
/// # Example
///
/// ```rust
/// use drasi_server_core::config::{ReactionConfig, ReactionSpecificConfig};
/// use drasi_server_core::config::typed::{HttpAdaptiveReactionConfig, AdaptiveBatchConfig};
/// use std::collections::HashMap;
///
/// let config = ReactionConfig {
///     id: "adaptive-webhook".to_string(),
///     queries: vec!["user-changes".to_string()],
///     auto_start: true,
///     config: ReactionSpecificConfig::HttpAdaptive(HttpAdaptiveReactionConfig {
///         base_url: "https://api.example.com".to_string(),
///         token: Some("your-api-token".to_string()),
///         timeout_ms: 10000,
///         routes: HashMap::new(),
///         adaptive: AdaptiveBatchConfig {
///             adaptive_min_batch_size: 20,
///             adaptive_max_batch_size: 500,
///             adaptive_window_size: 10,  // 1 second
///             adaptive_batch_timeout_ms: 1000,
///         },
///     }),
///     priority_queue_capacity: None,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpAdaptiveReactionConfig {
    /// Base URL for HTTP requests (e.g., "https://api.example.com")
    ///
    /// Batch requests are sent to `{base_url}/batch`
    #[serde(default = "default_base_url")]
    pub base_url: String,

    /// Optional bearer token for authentication
    ///
    /// If provided, adds `Authorization: Bearer {token}` header to all requests
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Query-specific route configurations for individual requests
    ///
    /// Used when only single results are available (fallback from batch endpoint).
    /// Maps query IDs to operation-specific call specifications.
    #[serde(default)]
    pub routes: HashMap<String, HttpQueryConfig>,

    /// Adaptive batching configuration (flattened into parent config)
    #[serde(flatten)]
    pub adaptive: AdaptiveBatchConfig,
}

// =============================================================================
// Standard Reaction Configuration Types
// =============================================================================

/// gRPC reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrpcReactionConfig {
    /// gRPC server URL
    #[serde(default = "default_grpc_endpoint")]
    pub endpoint: String,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Batch size for bundling events
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// Batch flush timeout in milliseconds
    #[serde(default = "default_batch_flush_timeout_ms")]
    pub batch_flush_timeout_ms: u64,

    /// Maximum retries for failed requests
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Connection retry attempts
    #[serde(default = "default_connection_retry_attempts")]
    pub connection_retry_attempts: u32,

    /// Initial connection timeout in milliseconds
    #[serde(default = "default_initial_connection_timeout_ms")]
    pub initial_connection_timeout_ms: u64,

    /// Metadata headers to include in requests
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

fn default_grpc_endpoint() -> String {
    "grpc://localhost:50052".to_string()
}

fn default_batch_flush_timeout_ms() -> u64 {
    1000
}

fn default_max_retries() -> u32 {
    3
}

fn default_connection_retry_attempts() -> u32 {
    5
}

fn default_initial_connection_timeout_ms() -> u64 {
    10000
}

/// SSE (Server-Sent Events) reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SseReactionConfig {
    /// Host to bind SSE server
    #[serde(default = "default_sse_host")]
    pub host: String,

    /// Port to bind SSE server
    #[serde(default = "default_sse_port")]
    pub port: u16,

    /// SSE path
    #[serde(default = "default_sse_path")]
    pub sse_path: String,

    /// Heartbeat interval in milliseconds
    #[serde(default = "default_heartbeat_interval_ms")]
    pub heartbeat_interval_ms: u64,
}

fn default_sse_path() -> String {
    "/events".to_string()
}

fn default_heartbeat_interval_ms() -> u64 {
    30000
}

fn default_sse_port() -> u16 {
    8080
}

fn default_sse_host() -> String {
    "0.0.0.0".to_string()
}

/// Platform reaction configuration (Redis Streams publishing)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlatformReactionConfig {
    /// Redis connection URL
    pub redis_url: String,

    /// PubSub name/topic
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pubsub_name: Option<String>,

    /// Source name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,

    /// Maximum stream length
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_stream_length: Option<usize>,

    /// Whether to emit control events
    #[serde(default)]
    pub emit_control_events: bool,

    /// Whether batching is enabled
    #[serde(default)]
    pub batch_enabled: bool,

    /// Maximum batch size
    #[serde(default = "default_batch_size")]
    pub batch_max_size: usize,

    /// Maximum batch wait time in milliseconds
    #[serde(default = "default_batch_wait_ms")]
    pub batch_max_wait_ms: u64,
}

fn default_batch_wait_ms() -> u64 {
    100
}

/// Profiler reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfilerReactionConfig {
    /// Window size for profiling statistics
    #[serde(default = "default_profiler_window_size")]
    pub window_size: usize,

    /// Report interval in seconds
    #[serde(default = "default_report_interval_secs")]
    pub report_interval_secs: u64,
}

fn default_profiler_window_size() -> usize {
    1000
}

fn default_report_interval_secs() -> u64 {
    60
}

/// Application reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplicationReactionConfig {
    /// Application-specific properties (for now, keep flexible)
    #[serde(flatten)]
    pub properties: HashMap<String, serde_json::Value>,
}

/// Enum wrapping all reaction-specific configurations
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "reaction_type", rename_all = "lowercase")]
pub enum ReactionSpecificConfig {
    Log(LogReactionConfig),
    Http(HttpReactionConfig),
    Grpc(GrpcReactionConfig),
    Sse(SseReactionConfig),
    Platform(PlatformReactionConfig),
    Profiler(ProfilerReactionConfig),
    Application(ApplicationReactionConfig),

    /// gRPC Adaptive reaction with adaptive batching
    #[serde(rename = "grpc_adaptive")]
    GrpcAdaptive(GrpcAdaptiveReactionConfig),

    /// HTTP Adaptive reaction with adaptive batching
    #[serde(rename = "http_adaptive")]
    HttpAdaptive(HttpAdaptiveReactionConfig),

    /// Custom reaction type with properties map (for extensibility)
    #[serde(rename = "custom")]
    Custom {
        #[serde(flatten)]
        properties: HashMap<String, serde_json::Value>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mock_source_config_defaults() {
        let config = MockSourceConfig::default();
        assert_eq!(config.data_type, "generic");
        assert_eq!(config.interval_ms, 5000);
    }

    #[test]
    fn test_mock_source_config_serialization() {
        let config = MockSourceConfig {
            data_type: "counter".to_string(),
            interval_ms: 1000,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: MockSourceConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_source_specific_config_enum() {
        let config = SourceSpecificConfig::Mock(MockSourceConfig {
            data_type: "sensor".to_string(),
            interval_ms: 2000,
        });

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"source_type\":\"mock\""));

        let deserialized: SourceSpecificConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, SourceSpecificConfig::Mock(_)));
    }

    #[test]
    fn test_log_reaction_config_defaults() {
        let config = LogReactionConfig::default();
        assert_eq!(config.log_level, LogLevel::Info);
    }

    #[test]
    fn test_reaction_specific_config_enum() {
        let config = ReactionSpecificConfig::Log(LogReactionConfig {
            log_level: LogLevel::Debug,
        });

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"reaction_type\":\"log\""));

        let deserialized: ReactionSpecificConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, ReactionSpecificConfig::Log(_)));
    }

    #[test]
    fn test_custom_source_config() {
        let mut properties = HashMap::new();
        properties.insert(
            "custom_field".to_string(),
            serde_json::Value::String("value".to_string()),
        );

        let config = SourceSpecificConfig::Custom { properties };

        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"source_type\":\"custom\""));
        assert!(json.contains("\"custom_field\":\"value\""));
    }
}
