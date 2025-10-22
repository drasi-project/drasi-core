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
use log::{error, info, warn};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::RwLock;

use crate::channels::priority_queue::PriorityQueue;
use crate::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, QueryResult,
};
use crate::config::ReactionConfig;
use crate::profiling::ProfilingMetadata;
use crate::reactions::Reaction;
use crate::server_core::DrasiServerCore;

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

        values.sort_by(|a, b| a.partial_cmp(b).unwrap());

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
    config: ReactionConfig,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: ComponentEventSender,
    stats: Arc<RwLock<ProfilingStats>>,
    report_interval_secs: u64,
    subscription_tasks: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
    priority_queue: PriorityQueue<QueryResult>,
    processing_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl ProfilerReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        let window_size = config
            .properties
            .get("window_size")
            .and_then(|v| v.as_u64())
            .unwrap_or(1000) as usize;

        let report_interval_secs = config
            .properties
            .get("report_interval_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(10);

        Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
            stats: Arc::new(RwLock::new(ProfilingStats::new(window_size))),
            report_interval_secs,
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            priority_queue: PriorityQueue::new(10000),
            processing_task: Arc::new(RwLock::new(None)),
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
    async fn start(&self, server_core: Arc<DrasiServerCore>) -> Result<()> {
        info!("Starting ProfilerReaction: {}", self.config.id);

        *self.status.write().await = ComponentStatus::Starting;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting profiler reaction".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Get QueryManager from server_core
        let query_manager = server_core.query_manager();

        // Subscribe to each query and spawn forwarder tasks
        for query_id in &self.config.queries {
            info!("[{}] Subscribing to query: {}", self.config.id, query_id);

            // Get the query instance
            let query = query_manager
                .get_query_instance(query_id)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get query instance {}: {}", query_id, e))?;

            // Subscribe to the query
            let response = query
                .subscribe(self.config.id.clone())
                .await
                .map_err(|e| anyhow::anyhow!("Failed to subscribe to query {}: {}", query_id, e))?;

            let mut broadcast_receiver = response.broadcast_receiver;
            let priority_queue = self.priority_queue.clone();
            let reaction_id = self.config.id.clone();
            let query_id_clone = query_id.clone();

            // Spawn a forwarder task for this query
            let forwarder_task = tokio::spawn(async move {
                info!(
                    "[{}] Forwarder task started for query: {}",
                    reaction_id, query_id_clone
                );

                loop {
                    match broadcast_receiver.recv().await {
                        Ok(query_result) => {
                            // Enqueue the result to the priority queue
                            if !priority_queue.enqueue(query_result.clone()).await {
                                warn!(
                                    "[{}] Priority queue full, dropping result from query: {}",
                                    reaction_id, query_id_clone
                                );
                            }
                        }
                        Err(RecvError::Lagged(count)) => {
                            warn!(
                                "[{}] Broadcast receiver lagged by {} messages for query: {}",
                                reaction_id, count, query_id_clone
                            );
                            continue;
                        }
                        Err(RecvError::Closed) => {
                            info!(
                                "[{}] Broadcast channel closed for query: {}",
                                reaction_id, query_id_clone
                            );
                            break;
                        }
                    }
                }

                info!(
                    "[{}] Forwarder task stopped for query: {}",
                    reaction_id, query_id_clone
                );
            });

            // Store the forwarder task handle
            self.subscription_tasks.write().await.push(forwarder_task);
        }

        *self.status.write().await = ComponentStatus::Running;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: Some("Profiler reaction started".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        info!(
            "[{}] Profiler started - window_size: {}, report_interval: {}s",
            self.config.id,
            self.stats.read().await.window_size,
            self.report_interval_secs
        );

        // Spawn the processing task
        let reaction_name = self.config.id.clone();
        let stats = self.stats.clone();
        let report_interval = self.report_interval_secs;
        let priority_queue = self.priority_queue.clone();

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
        *self.processing_task.write().await = Some(processing_task);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping ProfilerReaction: {}", self.config.id);
        *self.status.write().await = ComponentStatus::Stopped;

        // Abort all subscription forwarder tasks
        let mut tasks = self.subscription_tasks.write().await;
        for task in tasks.drain(..) {
            task.abort();
        }
        drop(tasks);

        // Abort the processing task
        let mut processing_task = self.processing_task.write().await;
        if let Some(task) = processing_task.take() {
            task.abort();
        }
        drop(processing_task);

        // Drain the priority queue
        let drained = self.priority_queue.drain().await;
        info!(
            "[{}] Drained {} events from priority queue",
            self.config.id,
            drained.len()
        );

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: Some("Profiler reaction stopped".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &ReactionConfig {
        &self.config
    }
}
