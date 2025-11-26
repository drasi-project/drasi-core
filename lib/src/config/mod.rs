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

// Re-export runtime types
pub use runtime::{QueryRuntime, ReactionRuntime, RuntimeConfig, SourceRuntime};

// Re-export config enums for plugin deserialization
pub use enums::{SourceSpecificConfig, ReactionSpecificConfig};

// Re-export schema types
pub use schema::*;

// Convenience re-exports for config types that remain in core

// Common reaction configs
pub use crate::reactions::common::AdaptiveBatchConfig;
