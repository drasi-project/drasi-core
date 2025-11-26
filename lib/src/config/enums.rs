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

//! Discriminated union enums for source and reaction configurations
//!
//! These enums allow for dynamic deserialization of different source and reaction types
//! from YAML/JSON configuration files using serde's tag-based discriminators.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Source-specific configuration enum
///
/// This enum acts as a discriminated union for all source types supported by the system.
/// The `source_type` field in the configuration determines which variant is deserialized.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "source_type", rename_all = "lowercase")]
pub enum SourceSpecificConfig {
    /// Mock source for testing
    Mock(HashMap<String, serde_json::Value>),
    /// PostgreSQL source
    Postgres(HashMap<String, serde_json::Value>),
    /// HTTP source
    Http(HashMap<String, serde_json::Value>),
    /// gRPC source
    Grpc(HashMap<String, serde_json::Value>),
    /// Platform source
    Platform(HashMap<String, serde_json::Value>),
    /// Application source
    Application(HashMap<String, serde_json::Value>),
    /// Custom source type for extensions
    #[serde(rename = "custom")]
    Custom {
        #[serde(flatten)]
        properties: HashMap<String, serde_json::Value>,
    },
}

/// Reaction-specific configuration enum
///
/// This enum acts as a discriminated union for all reaction types supported by the system.
/// The `reaction_type` field in the configuration determines which variant is deserialized.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reaction_type", rename_all = "lowercase")]
pub enum ReactionSpecificConfig {
    /// Log reaction for debugging
    Log(HashMap<String, serde_json::Value>),
    /// HTTP reaction
    Http(HashMap<String, serde_json::Value>),
    /// gRPC reaction
    Grpc(HashMap<String, serde_json::Value>),
    /// SSE reaction
    Sse(HashMap<String, serde_json::Value>),
    /// Platform reaction
    Platform(HashMap<String, serde_json::Value>),
    /// Profiler reaction
    Profiler(HashMap<String, serde_json::Value>),
    /// Application reaction
    Application(HashMap<String, serde_json::Value>),
    /// gRPC Adaptive reaction
    #[serde(rename = "grpc_adaptive")]
    GrpcAdaptive(HashMap<String, serde_json::Value>),
    /// HTTP Adaptive reaction
    #[serde(rename = "http_adaptive")]
    HttpAdaptive(HashMap<String, serde_json::Value>),
    /// Custom reaction type for extensions
    #[serde(rename = "custom")]
    Custom {
        #[serde(flatten)]
        properties: HashMap<String, serde_json::Value>,
    },
}