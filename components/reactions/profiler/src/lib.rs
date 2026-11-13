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

//! Profiler reaction plugin for Drasi
//!
//! This plugin implements Profiler reactions for Drasi.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_profiler::ProfilerReaction;
//!
//! let reaction = ProfilerReaction::builder("my-profiler")
//!     .with_queries(vec!["query1".to_string()])
//!     .with_window_size(2000)
//!     .with_report_interval_secs(30)
//!     .build()?;
//! ```

pub mod config;
pub mod descriptor;
pub mod profiler;

pub use config::ProfilerReactionConfig;
pub use profiler::ProfilerReaction;

/// Builder for Profiler reaction
pub struct ProfilerReactionBuilder {
    id: drasi_lib::ComponentId,
    queries: Vec<drasi_lib::config::ComponentReference>,
    window_size: usize,
    report_interval_secs: u64,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl ProfilerReactionBuilder {
    /// Create a new Profiler reaction builder with the given ID
    pub fn new(id: drasi_lib::ComponentId) -> Self {
        Self {
            id: id.clone(),
            queries: Vec::new(),
            window_size: 1000,
            report_interval_secs: 60,
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Set explicit name or ID references for every query input.
    pub fn with_query_inputs(mut self, inputs: Vec<drasi_lib::config::ComponentReference>) -> Self {
        self.queries = inputs;
        self
    }

    /// Set all query inputs by their current names.
    pub fn with_queries_by_name(self, names: Vec<drasi_lib::ComponentName>) -> Self {
        self.with_query_inputs(
            names
                .into_iter()
                .map(drasi_lib::config::ComponentReference::by_name)
                .collect(),
        )
    }

    /// Set all query inputs by their immutable IDs.
    pub fn with_queries_by_id(self, ids: Vec<drasi_lib::ComponentId>) -> Self {
        self.with_query_inputs(
            ids.into_iter()
                .map(drasi_lib::config::ComponentReference::by_id)
                .collect(),
        )
    }

    /// Add a query input by its current name.
    pub fn with_query_by_name(mut self, query_name: drasi_lib::ComponentName) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_name(query_name));
        self
    }

    /// Add a query input by its immutable ID.
    pub fn with_query_by_id(mut self, query_id: drasi_lib::ComponentId) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_id(query_id));
        self
    }

    /// Set the window size for profiling statistics
    pub fn with_window_size(mut self, size: usize) -> Self {
        self.window_size = size;
        self
    }

    /// Set the report interval in seconds
    pub fn with_report_interval_secs(mut self, secs: u64) -> Self {
        self.report_interval_secs = secs;
        self
    }

    /// Set the priority queue capacity
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the reaction should auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: ProfilerReactionConfig) -> Self {
        self.window_size = config.window_size;
        self.report_interval_secs = config.report_interval_secs;
        self
    }

    /// Build the Profiler reaction
    pub fn build(self) -> anyhow::Result<ProfilerReaction> {
        let config = ProfilerReactionConfig {
            window_size: self.window_size,
            report_interval_secs: self.report_interval_secs,
        };

        Ok(ProfilerReaction::from_builder(
            self.id,
            self.queries,
            config,
            self.priority_queue_capacity,
            self.auto_start,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::Reaction;

    #[test]
    fn test_profiler_builder_defaults() {
        let reaction = ProfilerReactionBuilder::new(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID"),
        )
        .build()
        .unwrap();
        assert_eq!(
            reaction.id(),
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID")
        );
    }

    #[test]
    fn test_profiler_builder_custom() {
        let reaction = ProfilerReaction::builder(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID"),
        )
        .with_window_size(2000)
        .with_report_interval_secs(30)
        .with_queries_by_name(vec![
            drasi_lib::ComponentName::try_new("query1").expect("fixture component name")
        ])
        .build()
        .unwrap();

        assert_eq!(
            reaction.id(),
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID")
        );
        assert_eq!(
            reaction.query_inputs(),
            vec![drasi_lib::config::ComponentReference::by_name(
                drasi_lib::ComponentName::try_new("query1").expect("fixture component name")
            )]
        );
    }

    #[test]
    fn test_profiler_new_constructor() {
        let config = ProfilerReactionConfig::default();
        let reaction = ProfilerReaction::new(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID"),
            vec![drasi_lib::config::ComponentReference::by_name(
                drasi_lib::ComponentName::try_new("query1").expect("fixture component name"),
            )],
            config,
        );
        assert_eq!(
            reaction.id(),
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000021")
                .expect("fixture UUID")
        );
    }
}

/// Dynamic plugin entry point.
///
/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "profiler-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::ProfilerReactionDescriptor],
    bootstrap_descriptors = [],
);
