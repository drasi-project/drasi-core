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
    id: String,
    queries: Vec<String>,
    window_size: usize,
    report_interval_secs: u64,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl ProfilerReactionBuilder {
    /// Create a new Profiler reaction builder with the given ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            window_size: 1000,
            report_interval_secs: 60,
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Set the query IDs to subscribe to
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Add a query ID to subscribe to
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
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
        let reaction = ProfilerReactionBuilder::new("test-reaction")
            .build()
            .unwrap();
        assert_eq!(reaction.id(), "test-reaction");
    }

    #[test]
    fn test_profiler_builder_custom() {
        let reaction = ProfilerReaction::builder("test-reaction")
            .with_window_size(2000)
            .with_report_interval_secs(30)
            .with_queries(vec!["query1".to_string()])
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    }

    #[test]
    fn test_profiler_new_constructor() {
        let config = ProfilerReactionConfig::default();
        let reaction = ProfilerReaction::new("test-reaction", vec!["query1".to_string()], config);
        assert_eq!(reaction.id(), "test-reaction");
    }
}

/// Dynamic plugin entry point.
///
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin! {
    drasi_plugin_sdk::PluginRegistration::new()
        .with_reaction(Box::new(descriptor::ProfilerReactionDescriptor))
}
