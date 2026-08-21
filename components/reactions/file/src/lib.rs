#![allow(unexpected_cfgs)]
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

//! File reaction plugin for Drasi.
//!
//! `FileReaction` subscribes to one or more continuous queries and writes their
//! result diffs (`ADD`, `UPDATE`, `DELETE`, and aggregation updates) to the
//! filesystem. Each diff is rendered with an optional Handlebars template and
//! persisted according to the configured [`WriteMode`]:
//!
//! - [`WriteMode::Append`] — append each record to a per-query file.
//! - [`WriteMode::Overwrite`] — atomically replace a file with the latest record.
//! - [`WriteMode::PerChange`] — write each record to a unique file.
//!
//! When no template applies, the reaction emits the canonical camelCase output
//! envelope (`queryId`, `sequenceId`, `timestamp`, `operation`, `before`/`after`,
//! `metadata`). Filenames are also templated and sanitized for filesystem safety.
//!
//! Construct instances with [`FileReaction::builder`] and configure them through
//! [`FileReactionConfig`]. See the crate `README.md` for the full configuration
//! reference and template context keys.

mod config;
pub mod descriptor;
mod file;

pub use config::{FileReactionConfig, QueryConfig, TemplateSpec, WriteMode};
pub use file::{FileReaction, FileReactionBuilder};

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "file-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::FileReactionDescriptor],
    bootstrap_descriptors = [],
);
