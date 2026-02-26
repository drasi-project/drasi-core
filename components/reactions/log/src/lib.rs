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

//! Log Reaction Plugin for drasi-lib
//!
//! This plugin provides console logging of query results.
//!
//! ## Instance-based Usage
//!
//! ```rust,ignore
//! use drasi_reaction_log::LogReaction;
//! use drasi_lib::config::{ReactionConfig, ReactionSpecificConfig};
//! use std::sync::Arc;
//!
//! // Create configuration
//! let config = ReactionConfig {
//!     id: "my-log".to_string(),
//!     queries: vec!["query1".to_string()],
//!     config: ReactionSpecificConfig::Log(props),
//!     ..Default::default()
//! };
//!
//! // Create instance and add to DrasiLib
//! let reaction = Arc::new(LogReaction::new(config, event_tx));
//! drasi.add_reaction(reaction).await?;
//! ```

mod config;
pub mod descriptor;
mod log;

#[cfg(test)]
mod tests;

pub use config::{LogReactionConfig, QueryConfig, TemplateSpec};
pub use log::{LogReaction, LogReactionBuilder};

/// Dynamic plugin entry point (legacy dylib).
///

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "log-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::LogReactionDescriptor],
    bootstrap_descriptors = [],
);
