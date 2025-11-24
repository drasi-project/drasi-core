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

pub mod common;
pub mod enums;
pub mod runtime;
pub mod schema;

#[cfg(test)]
mod dispatch_mode_test;
#[cfg(test)]
mod tests;

// Re-export common types
pub use common::*;

// Re-export the new enum types (source of truth for config enums)
pub use enums::{ReactionSpecificConfig, SourceSpecificConfig};

// Re-export runtime types
pub use runtime::{QueryRuntime, ReactionRuntime, RuntimeConfig, SourceRuntime};

// Re-export schema types
pub use schema::*;

// Convenience re-exports for all config types so they're accessible via `use crate::config::ConfigName;`

// Source configs
pub use crate::sources::application::ApplicationSourceConfig;
pub use crate::sources::grpc::GrpcSourceConfig;
pub use crate::sources::http::HttpSourceConfig;
pub use crate::sources::mock::MockSourceConfig;
pub use crate::sources::platform::PlatformSourceConfig;
pub use crate::sources::postgres::PostgresSourceConfig;

// Reaction configs
pub use crate::reactions::application::ApplicationReactionConfig;
pub use crate::reactions::common::AdaptiveBatchConfig;
pub use crate::reactions::grpc::GrpcReactionConfig;
pub use crate::reactions::grpc_adaptive::GrpcAdaptiveReactionConfig;
pub use crate::reactions::http::{CallSpec, HttpReactionConfig};
// Note: QueryConfig is NOT re-exported to avoid collision with schema::QueryConfig
pub use crate::reactions::http_adaptive::HttpAdaptiveReactionConfig;
pub use crate::reactions::log::LogReactionConfig;
pub use crate::reactions::platform::PlatformReactionConfig;
pub use crate::reactions::profiler::ProfilerReactionConfig;
pub use crate::reactions::sse::SseReactionConfig;
