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

//! Performance profiling infrastructure for DrasiServerCore
//!
//! This module provides nanosecond-precision timestamp tracking through the entire
//! Source → Query → Reaction pipeline, enabling detailed performance analysis and
//! bottleneck identification.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Profiling metadata that tracks timestamps at each stage of event processing
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProfilingMetadata {
    /// Timestamp provided by the external source (if available)
    pub source_ns: Option<u64>,
    /// Timestamp when the source received the event
    pub source_receive_ns: Option<u64>,
    /// Timestamp when the source sent the event to the channel
    pub source_send_ns: Option<u64>,
    /// Timestamp when the query received the event
    pub query_receive_ns: Option<u64>,
    /// Timestamp before calling drasi-core processing
    pub query_core_call_ns: Option<u64>,
    /// Timestamp after drasi-core processing returned
    pub query_core_return_ns: Option<u64>,
    /// Timestamp when the query sent the result
    pub query_send_ns: Option<u64>,
    /// Timestamp when the reaction received the result
    pub reaction_receive_ns: Option<u64>,
    /// Timestamp when the reaction completed processing
    pub reaction_complete_ns: Option<u64>,
}

impl ProfilingMetadata {
    /// Create a new profiling metadata with the current timestamp as source_receive_ns
    pub fn new() -> Self {
        Self {
            source_receive_ns: Some(timestamp_ns()),
            ..Default::default()
        }
    }

    /// Create profiling metadata with an external source timestamp
    pub fn with_source_timestamp(source_ns: u64) -> Self {
        Self {
            source_ns: Some(source_ns),
            source_receive_ns: Some(timestamp_ns()),
            ..Default::default()
        }
    }

    /// Calculate elapsed time from source receive to query receive
    pub fn elapsed_source_to_query(&self) -> Option<u64> {
        match (self.source_send_ns, self.query_receive_ns) {
            (Some(send), Some(receive)) => Some(receive.saturating_sub(send)),
            _ => None,
        }
    }

    /// Calculate elapsed time for query processing (drasi-core)
    pub fn elapsed_query_processing(&self) -> Option<u64> {
        match (self.query_core_call_ns, self.query_core_return_ns) {
            (Some(call), Some(ret)) => Some(ret.saturating_sub(call)),
            _ => None,
        }
    }

    /// Calculate elapsed time from query send to reaction receive
    pub fn elapsed_query_to_reaction(&self) -> Option<u64> {
        match (self.query_send_ns, self.reaction_receive_ns) {
            (Some(send), Some(receive)) => Some(receive.saturating_sub(send)),
            _ => None,
        }
    }

    /// Calculate elapsed time for reaction processing
    pub fn elapsed_reaction_processing(&self) -> Option<u64> {
        match (self.reaction_receive_ns, self.reaction_complete_ns) {
            (Some(receive), Some(complete)) => Some(complete.saturating_sub(receive)),
            _ => None,
        }
    }

    /// Calculate total end-to-end elapsed time
    pub fn elapsed_total(&self) -> Option<u64> {
        let start = self.source_receive_ns.or(self.source_ns)?;
        let end = self.reaction_complete_ns.or(self.query_send_ns)?;
        Some(end.saturating_sub(start))
    }

    /// Calculate elapsed time within source (receive to send)
    pub fn elapsed_source_internal(&self) -> Option<u64> {
        match (self.source_receive_ns, self.source_send_ns) {
            (Some(receive), Some(send)) => Some(send.saturating_sub(receive)),
            _ => None,
        }
    }

    /// Calculate elapsed time within query (receive to send)
    pub fn elapsed_query_internal(&self) -> Option<u64> {
        match (self.query_receive_ns, self.query_send_ns) {
            (Some(receive), Some(send)) => Some(send.saturating_sub(receive)),
            _ => None,
        }
    }

    /// Get a summary of all elapsed times in milliseconds
    pub fn elapsed_summary_ms(&self) -> ProfilingElapsedSummary {
        ProfilingElapsedSummary {
            source_internal_ms: self
                .elapsed_source_internal()
                .map(|ns| ns as f64 / 1_000_000.0),
            source_to_query_ms: self
                .elapsed_source_to_query()
                .map(|ns| ns as f64 / 1_000_000.0),
            query_internal_ms: self
                .elapsed_query_internal()
                .map(|ns| ns as f64 / 1_000_000.0),
            query_processing_ms: self
                .elapsed_query_processing()
                .map(|ns| ns as f64 / 1_000_000.0),
            query_to_reaction_ms: self
                .elapsed_query_to_reaction()
                .map(|ns| ns as f64 / 1_000_000.0),
            reaction_processing_ms: self
                .elapsed_reaction_processing()
                .map(|ns| ns as f64 / 1_000_000.0),
            total_ms: self.elapsed_total().map(|ns| ns as f64 / 1_000_000.0),
        }
    }

    /// Check if profiling is enabled (has any timestamps)
    pub fn is_enabled(&self) -> bool {
        self.source_receive_ns.is_some()
            || self.source_ns.is_some()
            || self.source_send_ns.is_some()
            || self.query_receive_ns.is_some()
    }

    /// Merge another profiling metadata into this one, preserving existing values
    pub fn merge(&mut self, other: &ProfilingMetadata) {
        if self.source_ns.is_none() {
            self.source_ns = other.source_ns;
        }
        if self.source_receive_ns.is_none() {
            self.source_receive_ns = other.source_receive_ns;
        }
        if self.source_send_ns.is_none() {
            self.source_send_ns = other.source_send_ns;
        }
        if self.query_receive_ns.is_none() {
            self.query_receive_ns = other.query_receive_ns;
        }
        if self.query_core_call_ns.is_none() {
            self.query_core_call_ns = other.query_core_call_ns;
        }
        if self.query_core_return_ns.is_none() {
            self.query_core_return_ns = other.query_core_return_ns;
        }
        if self.query_send_ns.is_none() {
            self.query_send_ns = other.query_send_ns;
        }
        if self.reaction_receive_ns.is_none() {
            self.reaction_receive_ns = other.reaction_receive_ns;
        }
        if self.reaction_complete_ns.is_none() {
            self.reaction_complete_ns = other.reaction_complete_ns;
        }
    }
}

/// Summary of elapsed times in milliseconds
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilingElapsedSummary {
    pub source_internal_ms: Option<f64>,
    pub source_to_query_ms: Option<f64>,
    pub query_internal_ms: Option<f64>,
    pub query_processing_ms: Option<f64>,
    pub query_to_reaction_ms: Option<f64>,
    pub reaction_processing_ms: Option<f64>,
    pub total_ms: Option<f64>,
}

/// Configuration for profiling behavior
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilingConfig {
    /// Whether profiling is enabled globally
    pub enabled: bool,
    /// Sampling rate (0.0 to 1.0) - what percentage of events to profile
    pub sampling_rate: f64,
    /// Whether to include bootstrap events in profiling
    pub include_bootstrap: bool,
    /// Configuration for profiler reactions
    pub profiler_reactions: Vec<ProfilerReactionConfig>,
}

impl Default for ProfilingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sampling_rate: 1.0,
            include_bootstrap: true,
            profiler_reactions: Vec::new(),
        }
    }
}

impl ProfilingConfig {
    /// Create a new profiling configuration with profiling enabled
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            sampling_rate: 1.0,
            include_bootstrap: true,
            profiler_reactions: Vec::new(),
        }
    }

    /// Set the sampling rate (0.0 to 1.0)
    pub fn with_sampling_rate(mut self, rate: f64) -> Self {
        self.sampling_rate = rate.clamp(0.0, 1.0);
        self
    }

    /// Set whether to include bootstrap events
    pub fn with_include_bootstrap(mut self, include: bool) -> Self {
        self.include_bootstrap = include;
        self
    }

    /// Add a profiler reaction configuration
    pub fn add_profiler_reaction(mut self, config: ProfilerReactionConfig) -> Self {
        self.profiler_reactions.push(config);
        self
    }

    /// Check if we should profile this event based on sampling rate
    pub fn should_profile(&self) -> bool {
        if !self.enabled {
            return false;
        }
        if self.sampling_rate >= 1.0 {
            return true;
        }
        if self.sampling_rate <= 0.0 {
            return false;
        }
        // Simple sampling using random number
        rand::random::<f64>() < self.sampling_rate
    }
}

/// Configuration for a profiler reaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilerReactionConfig {
    pub query_id: String,
    pub output_format: OutputFormat,
    pub output_interval_seconds: Option<u64>,
    pub output_interval_events: Option<usize>,
    pub output_destination: OutputDestination,
}

/// Output format for profiling data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OutputFormat {
    HumanReadable,
    Csv,
    Json,
}

/// Output destination for profiling data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OutputDestination {
    Stdout,
    File(String),
    Both(String),
}

/// Get current timestamp in nanoseconds since UNIX epoch
pub fn timestamp_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("System time is before UNIX epoch")
        .as_nanos() as u64
}

/// Convert nanoseconds to milliseconds with decimal precision
pub fn ns_to_ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

/// Convert nanoseconds to seconds with decimal precision
pub fn ns_to_secs(ns: u64) -> f64 {
    ns as f64 / 1_000_000_000.0
}

/// Format nanoseconds as a human-readable duration string
pub fn format_duration_ns(ns: u64) -> String {
    if ns < 1_000 {
        format!("{}ns", ns)
    } else if ns < 1_000_000 {
        format!("{:.2}µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2}ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", ns as f64 / 1_000_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profiling_metadata_new() {
        let metadata = ProfilingMetadata::new();
        assert!(metadata.source_receive_ns.is_some());
        assert!(metadata.source_ns.is_none());
    }

    #[test]
    fn test_profiling_metadata_with_source_timestamp() {
        let source_ts = 1000000000;
        let metadata = ProfilingMetadata::with_source_timestamp(source_ts);
        assert_eq!(metadata.source_ns, Some(source_ts));
        assert!(metadata.source_receive_ns.is_some());
    }

    #[test]
    fn test_elapsed_calculations() {
        let mut metadata = ProfilingMetadata::default();

        // Set up timestamps for testing
        metadata.source_receive_ns = Some(1000);
        metadata.source_send_ns = Some(2000);
        metadata.query_receive_ns = Some(3000);
        metadata.query_core_call_ns = Some(3500);
        metadata.query_core_return_ns = Some(5500);
        metadata.query_send_ns = Some(6000);
        metadata.reaction_receive_ns = Some(7000);
        metadata.reaction_complete_ns = Some(8000);

        // Test individual elapsed time calculations
        assert_eq!(metadata.elapsed_source_internal(), Some(1000));
        assert_eq!(metadata.elapsed_source_to_query(), Some(1000));
        assert_eq!(metadata.elapsed_query_processing(), Some(2000));
        assert_eq!(metadata.elapsed_query_internal(), Some(3000));
        assert_eq!(metadata.elapsed_query_to_reaction(), Some(1000));
        assert_eq!(metadata.elapsed_reaction_processing(), Some(1000));
        assert_eq!(metadata.elapsed_total(), Some(7000));
    }

    #[test]
    fn test_elapsed_with_missing_timestamps() {
        let metadata = ProfilingMetadata::default();

        assert_eq!(metadata.elapsed_source_to_query(), None);
        assert_eq!(metadata.elapsed_query_processing(), None);
        assert_eq!(metadata.elapsed_query_to_reaction(), None);
        assert_eq!(metadata.elapsed_reaction_processing(), None);
        assert_eq!(metadata.elapsed_total(), None);
    }

    #[test]
    fn test_elapsed_summary_ms() {
        let mut metadata = ProfilingMetadata::default();

        metadata.source_receive_ns = Some(1_000_000); // 1ms
        metadata.source_send_ns = Some(2_000_000); // 2ms
        metadata.query_receive_ns = Some(3_000_000); // 3ms
        metadata.query_core_call_ns = Some(3_500_000); // 3.5ms
        metadata.query_core_return_ns = Some(5_500_000); // 5.5ms
        metadata.query_send_ns = Some(6_000_000); // 6ms
        metadata.reaction_receive_ns = Some(7_000_000); // 7ms
        metadata.reaction_complete_ns = Some(8_000_000); // 8ms

        let summary = metadata.elapsed_summary_ms();

        assert_eq!(summary.source_internal_ms, Some(1.0));
        assert_eq!(summary.source_to_query_ms, Some(1.0));
        assert_eq!(summary.query_processing_ms, Some(2.0));
        assert_eq!(summary.query_internal_ms, Some(3.0));
        assert_eq!(summary.query_to_reaction_ms, Some(1.0));
        assert_eq!(summary.reaction_processing_ms, Some(1.0));
        assert_eq!(summary.total_ms, Some(7.0));
    }

    #[test]
    fn test_is_enabled() {
        let mut metadata = ProfilingMetadata::default();
        assert!(!metadata.is_enabled());

        metadata.source_receive_ns = Some(1000);
        assert!(metadata.is_enabled());
    }

    #[test]
    fn test_merge_metadata() {
        let mut metadata1 = ProfilingMetadata::default();
        metadata1.source_receive_ns = Some(1000);
        metadata1.source_send_ns = Some(2000);

        let mut metadata2 = ProfilingMetadata::default();
        metadata2.query_receive_ns = Some(3000);
        metadata2.query_send_ns = Some(4000);
        metadata2.source_send_ns = Some(9999); // Should not override

        metadata1.merge(&metadata2);

        assert_eq!(metadata1.source_receive_ns, Some(1000));
        assert_eq!(metadata1.source_send_ns, Some(2000)); // Not overridden
        assert_eq!(metadata1.query_receive_ns, Some(3000)); // Merged
        assert_eq!(metadata1.query_send_ns, Some(4000)); // Merged
    }

    #[test]
    fn test_profiling_config_default() {
        let config = ProfilingConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.sampling_rate, 1.0);
        assert!(config.include_bootstrap);
    }

    #[test]
    fn test_profiling_config_sampling_rate() {
        let config = ProfilingConfig::enabled()
            .with_sampling_rate(0.5)
            .with_include_bootstrap(false);

        assert!(config.enabled);
        assert_eq!(config.sampling_rate, 0.5);
        assert!(!config.include_bootstrap);

        // Test clamping
        let config = ProfilingConfig::enabled().with_sampling_rate(1.5);
        assert_eq!(config.sampling_rate, 1.0);

        let config = ProfilingConfig::enabled().with_sampling_rate(-0.5);
        assert_eq!(config.sampling_rate, 0.0);
    }

    #[test]
    fn test_format_duration_ns() {
        assert_eq!(format_duration_ns(500), "500ns");
        assert_eq!(format_duration_ns(1_500), "1.50µs");
        assert_eq!(format_duration_ns(1_500_000), "1.50ms");
        assert_eq!(format_duration_ns(1_500_000_000), "1.50s");
    }

    #[test]
    fn test_ns_conversions() {
        assert_eq!(ns_to_ms(1_000_000), 1.0);
        assert_eq!(ns_to_ms(1_500_000), 1.5);
        assert_eq!(ns_to_secs(1_000_000_000), 1.0);
        assert_eq!(ns_to_secs(1_500_000_000), 1.5);
    }

    #[test]
    fn test_timestamp_ns() {
        let ts1 = timestamp_ns();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let ts2 = timestamp_ns();
        assert!(ts2 > ts1);
    }

    #[test]
    fn test_should_profile() {
        let config = ProfilingConfig {
            enabled: false,
            sampling_rate: 1.0,
            include_bootstrap: true,
            profiler_reactions: Vec::new(),
        };
        assert!(!config.should_profile());

        let config = ProfilingConfig {
            enabled: true,
            sampling_rate: 0.0,
            include_bootstrap: true,
            profiler_reactions: Vec::new(),
        };
        assert!(!config.should_profile());

        let config = ProfilingConfig {
            enabled: true,
            sampling_rate: 1.0,
            include_bootstrap: true,
            profiler_reactions: Vec::new(),
        };
        assert!(config.should_profile());
    }
}