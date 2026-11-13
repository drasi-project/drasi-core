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
use log::{error, info};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

use drasi_lib::channels::ComponentStatus;
use drasi_lib::observability::log_component_start;
use drasi_lib::profiling::ProfilingMetadata;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::common::CheckpointState;
use drasi_lib::Reaction;
use std::collections::HashMap;

pub use super::config::ProfilerReactionConfig;

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    use drasi_lib::{
        channels::{QueryResult, ResultDiff},
        context::{workers::WorkerAlreadyOwned, ReactionRuntimeContext},
        state_store::{MemoryStateStoreProvider, StateStoreProvider},
        ComponentId, ReactionCheckpoint, StorePartition,
    };
    use std::time::Duration;

    async fn fixture() -> Result<(ProfilerReaction, Arc<MemoryStateStoreProvider>, ComponentId)> {
        let query = ComponentId::generate();
        let reaction = ProfilerReaction::builder(ComponentId::generate())
            .with_query_by_id(query.clone())
            .build()?;
        let store = Arc::new(MemoryStateStoreProvider::new());
        let (updates, _rx) = tokio::sync::mpsc::channel(32);
        reaction
            .bind_runtime_context(ReactionRuntimeContext::new(
                "profiler-checkpoints",
                reaction.id().clone(),
                vec![query.clone()],
                Some(store.clone()),
                updates,
                None,
            ))
            .await;
        Ok((reaction, store, query))
    }

    fn sample(query: &ComponentId, sequence: u64) -> QueryResult {
        let mut result = QueryResult::new(
            query.clone(),
            sequence,
            chrono::Utc::now(),
            vec![ResultDiff::Noop],
            HashMap::new(),
        );
        result.profiling = Some(ProfilingMetadata::new());
        result
    }

    #[tokio::test(flavor = "current_thread")]
    async fn held_samples_checkpoint_only_after_completion_and_survive_cancelled_stop() -> Result<()>
    {
        let (reaction, _store, query) = fixture().await?;
        reaction.start().await?;
        tokio::task::yield_now().await;
        let seed = ReactionCheckpoint {
            sequence: 0,
            config_hash: 91,
        };
        reaction.base.write_checkpoint(&query, &seed).await?;
        let held = reaction.stats.read().await;
        reaction.enqueue_query_result(sample(&query, 1)).await?;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !reaction.base.priority_queue.is_empty().await {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(held.count, 0);
        assert_eq!(reaction.base.read_checkpoint(&query).await?, Some(seed));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), reaction.stop())
                .await
                .is_err()
        );
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .is::<WorkerAlreadyOwned>());
        assert!(reaction.base.processing_task.read().await.is_some());
        drop(held);
        reaction.stop().await?;
        assert_eq!(reaction.stats.read().await.count, 1);
        assert_eq!(
            reaction.base.read_checkpoint(&query).await?,
            Some(ReactionCheckpoint {
                sequence: 1,
                config_hash: 91
            })
        );
        assert!(reaction.base.processing_task.read().await.is_none());
        reaction.start().await?;
        let mut no_sample = sample(&query, 2);
        no_sample.profiling = None;
        reaction.enqueue_query_result(no_sample).await?;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if reaction
                    .base
                    .read_checkpoint(&query)
                    .await?
                    .is_some_and(|cp| cp.sequence == 2)
                {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        assert_eq!(reaction.stats.read().await.count, 1);
        reaction.stop().await?;
        assert_eq!(
            reaction.base.read_checkpoint(&query).await?,
            Some(ReactionCheckpoint {
                sequence: 2,
                config_hash: 91
            })
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn checkpoint_read_failure_preserves_old_state_and_stops_later_samples() -> Result<()> {
        let (mut reaction, store, query) = fixture().await?;
        reaction.base.recovery_policy = Some(drasi_lib::ReactionRecoveryPolicy::Strict);
        let partition = StorePartition::component(reaction.id().clone());
        let key = format!("checkpoint:{query}");
        store.set(&partition, &key, vec![0]).await?;
        reaction.start().await?;
        reaction.enqueue_query_result(sample(&query, 1)).await?;
        tokio::time::timeout(Duration::from_secs(1), async {
            while reaction.status().await != ComponentStatus::Error {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(reaction.stats.read().await.count, 1);
        assert_eq!(store.get(&partition, &key).await?, Some(vec![0]));
        reaction.enqueue_query_result(sample(&query, 2)).await?;
        tokio::task::yield_now().await;
        assert_eq!(reaction.stats.read().await.count, 1);
        assert!(reaction.base.processing_task.read().await.is_some());
        reaction.stop().await?;
        assert!(reaction.base.processing_task.read().await.is_none());
        Ok(())
    }
}

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
        let mut values: Vec<f64> = self.samples.iter().filter_map(&extract_fn).collect();

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

use super::ProfilerReactionBuilder;

/// ProfilerReaction collects and analyzes profiling data
pub struct ProfilerReaction {
    base: ReactionBase,
    cleanup_required: Mutex<bool>,
    config: ProfilerReactionConfig,
    stats: Arc<RwLock<ProfilingStats>>,
    report_interval_secs: u64,
}

impl ProfilerReaction {
    /// Create a builder for ProfilerReaction
    pub fn builder(id: drasi_lib::ComponentId) -> ProfilerReactionBuilder {
        ProfilerReactionBuilder::new(id)
    }

    /// Create a new profiler reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(
        id: drasi_lib::ComponentId,
        queries: Vec<drasi_lib::config::ComponentReference>,
        config: ProfilerReactionConfig,
    ) -> Self {
        Self::create_internal(id.clone(), queries, config, None, true)
    }

    /// Create from builder (internal method)
    pub(crate) fn from_builder(
        id: drasi_lib::ComponentId,
        queries: Vec<drasi_lib::config::ComponentReference>,
        config: ProfilerReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        Self::create_internal(id, queries, config, priority_queue_capacity, auto_start)
    }

    /// Internal constructor
    fn create_internal(
        id: drasi_lib::ComponentId,
        queries: Vec<drasi_lib::config::ComponentReference>,
        config: ProfilerReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        let window_size = config.window_size;
        let report_interval_secs = config.report_interval_secs;

        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        Self {
            base: ReactionBase::new(params),
            cleanup_required: Mutex::new(false),
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
    fn id(&self) -> &drasi_lib::ComponentId {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "profiler"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        use crate::descriptor::ProfilerReactionConfigDto;
        use drasi_plugin_sdk::ConfigValue;

        let dto = ProfilerReactionConfigDto {
            window_size: Some(ConfigValue::Static(self.config.window_size)),
            report_interval_secs: Some(ConfigValue::Static(self.config.report_interval_secs)),
        };

        match serde_json::to_value(&dto) {
            Ok(serde_json::Value::Object(map)) => map.into_iter().collect(),
            _ => HashMap::new(),
        }
    }

    fn query_inputs(&self) -> Vec<drasi_lib::config::ComponentReference> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn bind_runtime_context(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.bind_runtime_context(context).await;
    }

    async fn start(&self) -> Result<()> {
        let mut cleanup_required = self.cleanup_required.lock().await;
        if *cleanup_required || self.base.processing_task.read().await.is_some() {
            return Err(drasi_lib::context::workers::WorkerAlreadyOwned.into());
        }
        *cleanup_required = true;
        log_component_start("Reaction", &self.base.id);

        // Transition to Starting
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting profiler reaction".to_string()),
            )
            .await;

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
        let base = self.base.clone_shared();
        let policy = self
            .base
            .resolved_recovery_policy(self.default_recovery_policy());
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        drasi_lib::context::workers::spawn_owned_worker(&self.base.processing_task, async move {
            let mut checkpoints = CheckpointState::load(&base).await;
            let mut report_timer =
                tokio::time::interval(tokio::time::Duration::from_secs(report_interval));
            report_timer.tick().await; // Skip first immediate tick

            loop {
                let query_result = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => break,
                    result = async {
                        tokio::select! {
                            result = priority_queue.dequeue() => Some(result),
                            _ = report_timer.tick() => None,
                        }
                    } => result,
                };
                if let Some(query_result) = query_result {
                    if let Some(profiling) = query_result.profiling.clone() {
                        stats.write().await.add_sample(profiling);
                    }
                    if let Err(error) = checkpoints
                        .advance_with_policy(
                            &base,
                            &query_result.query_id,
                            query_result.sequence,
                            policy,
                        )
                        .await
                    {
                        error!("[{reaction_name}] Profiler checkpoint write failed: {error:#}");
                        base.set_status(
                            ComponentStatus::Error,
                            Some(format!(
                                "Profiler checkpoint failed for query '{}' (seq {}): {error:#}",
                                query_result.query_id, query_result.sequence
                            )),
                        )
                        .await;
                        break;
                    }
                } else {
                    let stats_guard = stats.read().await;

                    if stats_guard.count == 0 {
                        info!("[{reaction_name}] No profiling data collected yet");
                        continue;
                    }

                    info!("[{reaction_name}] ========== Profiling Report ==========");

                    let source_to_query = stats_guard.get_source_to_query_stats();
                    info!(
                        "[{}] {}",
                        reaction_name,
                        Self::format_stats("Source→Query", &source_to_query)
                    );

                    let query_processing = stats_guard.get_query_processing_stats();
                    info!(
                        "[{}] {}",
                        reaction_name,
                        Self::format_stats("Query Processing", &query_processing)
                    );

                    let query_to_reaction = stats_guard.get_query_to_reaction_stats();
                    info!(
                        "[{}] {}",
                        reaction_name,
                        Self::format_stats("Query→Reaction", &query_to_reaction)
                    );

                    let reaction_processing = stats_guard.get_reaction_processing_stats();
                    info!(
                        "[{}] {}",
                        reaction_name,
                        Self::format_stats("Reaction Processing", &reaction_processing)
                    );

                    let total = stats_guard.get_total_latency_stats();
                    info!(
                        "[{}] {}",
                        reaction_name,
                        Self::format_stats("Total End-to-End", &total)
                    );

                    info!("[{reaction_name}] ======================================");
                }
            }
        })
        .await?;
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Profiler reaction started".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let mut cleanup_required = self.cleanup_required.lock().await;
        *cleanup_required = true;
        self.base.stop_common_gracefully().await?;

        // Transition to Stopped
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Profiler reaction stopped".to_string()),
            )
            .await;
        *cleanup_required = false;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(
        &self,
        result: drasi_lib::channels::QueryResult,
    ) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }

    fn is_durable(&self) -> bool {
        false
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    fn default_recovery_policy(&self) -> drasi_lib::recovery::ReactionRecoveryPolicy {
        drasi_lib::recovery::ReactionRecoveryPolicy::AutoSkipGap
    }
}

#[cfg(test)]
mod tests;
