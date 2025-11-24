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

//! Configuration enum types for sources and reactions.
//!
//! This module contains discriminated union enums that enable polymorphic configuration
//! deserialization. Config structs will be imported as they are moved from typed.rs.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Config structs imported as they are moved
use crate::sources::application::ApplicationSourceConfig;
use crate::sources::grpc::GrpcSourceConfig;
use crate::sources::http::HttpSourceConfig;
use crate::sources::mock::MockSourceConfig;
use crate::sources::platform::PlatformSourceConfig;
use crate::sources::postgres::PostgresSourceConfig;

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

// Reaction config structs imported as they are moved
use crate::reactions::application::ApplicationReactionConfig;
use crate::reactions::grpc::GrpcReactionConfig;
use crate::reactions::grpc_adaptive::GrpcAdaptiveReactionConfig;
use crate::reactions::http::HttpReactionConfig;
use crate::reactions::http_adaptive::HttpAdaptiveReactionConfig;
use crate::reactions::log::LogReactionConfig;
use crate::reactions::platform::PlatformReactionConfig;
use crate::reactions::profiler::ProfilerReactionConfig;
use crate::reactions::sse::SseReactionConfig;

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
