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

use anyhow::Result;
use async_trait::async_trait;
use log::info;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::RwLock;

use drasi_lib::channels::{ComponentEventSender, ComponentStatus};
use drasi_lib::plugin_core::{QuerySubscriber, Reaction};
use drasi_lib::profiling::ProfilingMetadata;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::managers::log_component_start;
use std::collections::HashMap;

pub use super::config::ProfilerReactionConfig;

/// Statistics for a specific metric
#[derive(Debug, Clone)]
pub struct MetricStats {
    pub count: usize,
    pub mean: f64,
    pub variance: f64,
    pub std_dev: f64,
    pub min: f64,
    pub max: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

/// Profiling statistics aggregator using Welford's algorithm for online variance
struct ProfilingStats {
    window_size: usize,
    samples: VecDeque<ProfilingMetadata>,

    // Running statistics using Welford's algorithm
    count: usize,
    mean_source_to_query: f64,
    m2_source_to_query: f64,

    mean_query_processing: f64,
    m2_query_processing: f64,

    mean_query_to_reaction: f64,
    m2_query_to_reaction: f64,

    mean_reaction_processing: f64,
    m2_reaction_processing: f64,

    mean_total_latency: f64,
    m2_total_latency: f64,
}

impl ProfilingStats {
    fn new(window_size: usize) -> Self {
        Self {
            window_size,
            samples: VecDeque::with_capacity(window_size),
            count: 0,
            mean_source_to_query: 0.0,
            m2_source_to_query: 0.0,
            mean_query_processing: 0.0,
            m2_query_processing: 0.0,
            mean_query_to_reaction: 0.0,
            m2_query_to_reaction: 0.0,
            mean_reaction_processing: 0.0,
            m2_reaction_processing: 0.0,
            mean_total_latency: 0.0,
            m2_total_latency: 0.0,
        }
    }

    /// Add a new sample using Welford's algorithm for online variance
    fn add_sample(&mut self, profiling: ProfilingMetadata) {
        // Calculate latencies
        let source_to_query = if let (Some(send), Some(recv)) =
            (profiling.source_send_ns, profiling.query_receive_ns)
        {
            Some((recv - send) as f64)
        } else {
            None
        };

        let query_processing = if let (Some(call), Some(ret)) =
            (profiling.query_core_call_ns, profiling.query_core_return_ns)
        {
            Some((ret - call) as f64)
        } else {
            None
        };

        let query_to_reaction = if let (Some(send), Some(recv)) =
            (profiling.query_send_ns, profiling.reaction_receive_ns)
        {
            Some((recv - send) as f64)
        } else {
            None
        };

        let reaction_processing = if let (Some(recv), Some(complete)) = (
            profiling.reaction_receive_ns,
            profiling.reaction_complete_ns,
        ) {
            Some((complete - recv) as f64)
        } else {
            None
        };

        let total_latency = if let (Some(send), Some(complete)) =
            (profiling.source_send_ns, profiling.reaction_complete_ns)
        {
            Some((complete - send) as f64)
        } else {
            None
        };

        // Update Welford's algorithm for each metric
        self.count += 1;
        let n = self.count as f64;

        if let Some(val) = source_to_query {
            let delta = val - self.mean_source_to_query;
            self.mean_source_to_query += delta / n;
            let delta2 = val - self.mean_source_to_query;
            self.m2_source_to_query += delta * delta2;
        }

        if let Some(val) = query_processing {
            let delta = val - self.mean_query_processing;
            self.mean_query_processing += delta / n;
            let delta2 = val - self.mean_query_processing;
            self.m2_query_processing += delta * delta2;
        }

        if let Some(val) = query_to_reaction {
            let delta = val - self.mean_query_to_reaction;
            self.mean_query_to_reaction += delta / n;
            let delta2 = val - self.mean_query_to_reaction;
            self.m2_query_to_reaction += delta * delta2;
        }

        if let Some(val) = reaction_processing {
            let delta = val - self.mean_reaction_processing;
            self.mean_reaction_processing += delta / n;
            let delta2 = val - self.mean_reaction_processing;
            self.m2_reaction_processing += delta * delta2;
        }

        if let Some(val) = total_latency {
            let delta = val - self.mean_total_latency;
            self.mean_total_latency += delta / n;
            let delta2 = val - self.mean_total_latency;
            self.m2_total_latency += delta * delta2;
        }

        // Store sample in window
        self.samples.push_back(profiling);
        if self.samples.len() > self.window_size {
            self.samples.pop_front();
        }
    }

    /// Calculate percentiles from samples
    fn calculate_percentiles(
        &self,
        extract_fn: impl Fn(&ProfilingMetadata) -> Option<f64>,
    ) -> (f64, f64, f64, f64, f64) {
        let mut values: Vec<f64> = self.samples.iter().filter_map(|p| extract_fn(p)).collect();

        if values.is_empty() {
            return (0.0, 0.0, 0.0, 0.0, 0.0);
        }

        // Sort with NaN handling - NaN values are placed at the end
        values.sort_by(|a, b| {
            a.partial_cmp(b).unwrap_or_else(|| {
                if a.is_nan() && b.is_nan() {
                    std::cmp::Ordering::Equal
                } else if a.is_nan() {
                    std::cmp::Ordering::Greater // NaN goes to end
                } else {
                    std::cmp::Ordering::Less // b is NaN, goes to end
                }
            })
        });

        let min = values[0];
        let max = values[values.len() - 1];
        let p50 = percentile(&values, 0.50);
        let p95 = percentile(&values, 0.95);
        let p99 = percentile(&values, 0.99);

        (min, max, p50, p95, p99)
    }

    /// Get statistics for source-to-query latency
    fn get_source_to_query_stats(&self) -> MetricStats {
        let variance = if self.count > 1 {
            self.m2_source_to_query / (self.count - 1) as f64
        } else {
            0.0
        };

        let (min, max, p50, p95, p99) = self.calculate_percentiles(|p| {
            if let (Some(send), Some(recv)) = (p.source_send_ns, p.query_receive_ns) {
                Some((recv - send) as f64)
            } else {
                None
            }
        });

        MetricStats {
            count: self.count,
            mean: self.mean_source_to_query,
            variance,
            std_dev: variance.sqrt(),
            min,
            max,
            p50,
            p95,
            p99,
        }
    }

    /// Get statistics for query processing time
    fn get_query_processing_stats(&self) -> MetricStats {
        let variance = if self.count > 1 {
            self.m2_query_processing / (self.count - 1) as f64
        } else {
            0.0
        };

        let (min, max, p50, p95, p99) = self.calculate_percentiles(|p| {
            if let (Some(call), Some(ret)) = (p.query_core_call_ns, p.query_core_return_ns) {
                Some((ret - call) as f64)
            } else {
                None
            }
        });

        MetricStats {
            count: self.count,
            mean: self.mean_query_processing,
            variance,
            std_dev: variance.sqrt(),
            min,
            max,
            p50,
            p95,
            p99,
        }
    }

    /// Get statistics for query-to-reaction latency
    fn get_query_to_reaction_stats(&self) -> MetricStats {
        let variance = if self.count > 1 {
            self.m2_query_to_reaction / (self.count - 1) as f64
        } else {
            0.0
        };

        let (min, max, p50, p95, p99) = self.calculate_percentiles(|p| {
            if let (Some(send), Some(recv)) = (p.query_send_ns, p.reaction_receive_ns) {
                Some((recv - send) as f64)
            } else {
                None
            }
        });

        MetricStats {
            count: self.count,
            mean: self.mean_query_to_reaction,
            variance,
            std_dev: variance.sqrt(),
            min,
            max,
            p50,
            p95,
            p99,
        }
    }

    /// Get statistics for reaction processing time
    fn get_reaction_processing_stats(&self) -> MetricStats {
        let variance = if self.count > 1 {
            self.m2_reaction_processing / (self.count - 1) as f64
        } else {
            0.0
        };

        let (min, max, p50, p95, p99) = self.calculate_percentiles(|p| {
            if let (Some(recv), Some(complete)) = (p.reaction_receive_ns, p.reaction_complete_ns) {
                Some((complete - recv) as f64)
            } else {
                None
            }
        });

        MetricStats {
            count: self.count,
            mean: self.mean_reaction_processing,
            variance,
            std_dev: variance.sqrt(),
            min,
            max,
            p50,
            p95,
            p99,
        }
    }

    /// Get statistics for total end-to-end latency
    fn get_total_latency_stats(&self) -> MetricStats {
        let variance = if self.count > 1 {
            self.m2_total_latency / (self.count - 1) as f64
        } else {
            0.0
        };

        let (min, max, p50, p95, p99) = self.calculate_percentiles(|p| {
            if let (Some(send), Some(complete)) = (p.source_send_ns, p.reaction_complete_ns) {
                Some((complete - send) as f64)
            } else {
                None
            }
        });

        MetricStats {
            count: self.count,
            mean: self.mean_total_latency,
            variance,
            std_dev: variance.sqrt(),
            min,
            max,
            p50,
            p95,
            p99,
        }
    }
}

/// Calculate percentile from sorted values
fn percentile(sorted_values: &[f64], p: f64) -> f64 {
    let index = (p * (sorted_values.len() - 1) as f64).round() as usize;
    sorted_values[index]
}

/// ProfilerReaction collects and analyzes profiling data
pub struct ProfilerReaction {
    base: ReactionBase,
    config: ProfilerReactionConfig,
    stats: Arc<RwLock<ProfilingStats>>,
    report_interval_secs: u64,
}

impl ProfilerReaction {
    /// Create a new profiler reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: ProfilerReactionConfig) -> Self {
        let id = id.into();
        let window_size = config.window_size;
        let report_interval_secs = config.report_interval_secs;

        let params = ReactionBaseParams::new(id, queries);
        Self {
            base: ReactionBase::new(params),
            config,
            stats: Arc::new(RwLock::new(ProfilingStats::new(window_size))),
            report_interval_secs,
        }
    }

    fn format_stats(name: &str, stats: &MetricStats) -> String {
        format!(
            "{}: mean={:.2}ms, stddev={:.2}ms, min={:.2}ms, p50={:.2}ms, p95={:.2}ms, p99={:.2}ms, max={:.2}ms (n={})",
            name,
            stats.mean / 1_000_000.0,
            stats.std_dev / 1_000_000.0,
            stats.min / 1_000_000.0,
            stats.p50 / 1_000_000.0,
            stats.p95 / 1_000_000.0,
            stats.p99 / 1_000_000.0,
            stats.max / 1_000_000.0,
            stats.count
        )
    }
}

#[async_trait]
impl Reaction for ProfilerReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "profiler"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "window_size".to_string(),
            serde_json::Value::Number(self.config.window_size.into()),
        );
        props.insert(
            "report_interval_secs".to_string(),
            serde_json::Value::Number(self.config.report_interval_secs.into()),
        );
        props
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    async fn inject_query_subscriber(&self, query_subscriber: Arc<dyn QuerySubscriber>) {
        self.base.inject_query_subscriber(query_subscriber).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("Reaction", &self.base.id);

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting profiler reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        // QuerySubscriber was injected via inject_query_subscriber() when reaction was added
        self.base.subscribe_to_queries().await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Profiler reaction started".to_string()),
            )
            .await?;

        info!(
            "[{}] Profiler started - window_size: {}, report_interval: {}s",
            self.base.id,
            self.stats.read().await.window_size,
            self.report_interval_secs
        );

        // Spawn the processing task
        let reaction_name = self.base.id.clone();
        let stats = self.stats.clone();
        let report_interval = self.report_interval_secs;
        let priority_queue = self.base.priority_queue.clone();

        let processing_task = tokio::spawn(async move {
            let mut report_timer =
                tokio::time::interval(tokio::time::Duration::from_secs(report_interval));
            report_timer.tick().await; // Skip first immediate tick

            loop {
                tokio::select! {
                    query_result = priority_queue.dequeue() => {
                        // Extract and store profiling data
                        if let Some(profiling) = query_result.profiling.clone() {
                            stats.write().await.add_sample(profiling);
                        }
                    }
                    _ = report_timer.tick() => {
                        // Generate periodic report
                        let stats_guard = stats.read().await;

                        if stats_guard.count == 0 {
                            info!("[{}] No profiling data collected yet", reaction_name);
                            continue;
                        }

                        info!("[{}] ========== Profiling Report ==========", reaction_name);

                        let source_to_query = stats_guard.get_source_to_query_stats();
                        info!("[{}] {}", reaction_name, Self::format_stats("Source→Query", &source_to_query));

                        let query_processing = stats_guard.get_query_processing_stats();
                        info!("[{}] {}", reaction_name, Self::format_stats("Query Processing", &query_processing));

                        let query_to_reaction = stats_guard.get_query_to_reaction_stats();
                        info!("[{}] {}", reaction_name, Self::format_stats("Query→Reaction", &query_to_reaction));

                        let reaction_processing = stats_guard.get_reaction_processing_stats();
                        info!("[{}] {}", reaction_name, Self::format_stats("Reaction Processing", &reaction_processing));

                        let total = stats_guard.get_total_latency_stats();
                        info!("[{}] {}", reaction_name, Self::format_stats("Total End-to-End", &total));

                        info!("[{}] ======================================", reaction_name);
                    }
                }
            }
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_task).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Use ReactionBase common stop functionality
        self.base.stop_common().await?;

        // Transition to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Profiler reaction stopped".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}

#[cfg(test)]
mod tests;
