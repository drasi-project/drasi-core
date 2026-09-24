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

#![allow(unexpected_cfgs)]

//! Log Reaction Plugin for Drasi.
//!
//! This reaction writes continuous query results to the console (stdout),
//! which makes it well suited to development, debugging, and low-volume
//! monitoring. Output is emitted in timestamp order and can be customised with
//! per-query or default Handlebars templates; when no template applies, a
//! human-readable line is printed for every change.
//!
//! # Example
//!
//! ```rust
//! use drasi_reaction_log::{LogReaction, QueryConfig, TemplateSpec};
//!
//! let reaction = LogReaction::builder("my-log")
//!     .with_query("query1")
//!     .with_default_template(QueryConfig {
//!         added: Some(TemplateSpec::new("[ADD] {{after.id}}")),
//!         updated: Some(TemplateSpec::new("[UPD] {{after.id}}")),
//!         deleted: Some(TemplateSpec::new("[DEL] {{before.id}}")),
//!     })
//!     .build()
//!     .unwrap();
//!
//! // `reaction` is then registered with `DrasiLib` via `add_reaction()`.
//! # let _ = reaction;
//! ```

mod config;
pub mod descriptor;
mod log;
mod render;

#[cfg(test)]
mod tests;

pub use config::{LogReactionConfig, QueryConfig, TemplateSpec};
pub use log::{LogReaction, LogReactionBuilder};

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
