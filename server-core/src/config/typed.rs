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
    10
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

/// HTTP call specification for HTTP reaction
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallSpec {
    pub url: String,
    pub method: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// Query-specific HTTP call configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryCallConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<CallSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<CallSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<CallSpec>,
}

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
    pub queries: HashMap<String, QueryCallConfig>,
}

fn default_base_url() -> String {
    "http://localhost".to_string()
}

/// gRPC reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrpcReactionConfig {
    /// gRPC server URL
    #[serde(default = "default_grpc_endpoint")]
    pub endpoint: String,

    /// Optional authentication token
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

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
    /// Output file path for profiling data
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_file: Option<String>,

    /// Whether to include detailed timing breakdowns
    #[serde(default = "default_detailed")]
    pub detailed: bool,

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

fn default_detailed() -> bool {
    true
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
