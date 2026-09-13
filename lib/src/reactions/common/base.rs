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

//! Base implementation for common reaction functionality.
//!
//! This module provides `ReactionBase` which encapsulates common patterns
//! used across all reaction implementations:
//! - Query subscription management
//! - Priority queue handling
//! - Task lifecycle management
//! - Component status tracking
//! - Event reporting
//!
//! # Plugin Architecture
//!
//! ReactionBase is designed to be used by reaction plugins. Each plugin:
//! 1. Defines its own typed configuration struct
//! 2. Creates a ReactionBase with ReactionBaseParams
//! 3. Implements the Reaction trait delegating to ReactionBase methods

use anyhow::Result;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tracing::Instrument;

use crate::channels::priority_queue::PriorityQueue;
use crate::channels::{ComponentStatus, QueryResult};
use crate::component_graph::ComponentStatusHandle;
use crate::context::ReactionRuntimeContext;
use crate::identity::IdentityProvider;
use crate::reactions::checkpoint::ReactionCheckpoint;
use crate::recovery::ReactionRecoveryPolicy;
use crate::state_store::StateStoreProvider;

#[derive(Debug, thiserror::Error)]
#[error(
    "query result sequence inversion for query '{query_id}': received sequence {received} after {last_observed}"
)]
struct QueryResultSequenceInversion {
    query_id: String,
    received: u64,
    last_observed: u64,
}

struct QuerySequenceCursor {
    last_observed: u64,
    // Retained for this loop generation to distinguish unseen inversions from
    // old duplicates. Space is O(disjoint forward gaps), not the gap widths or
    // contiguous event count; evicting a gap would silently accept an inversion.
    missing: Vec<std::ops::Range<u64>>,
}

impl QuerySequenceCursor {
    fn new(checkpoint: u64) -> Self {
        Self {
            last_observed: checkpoint,
            missing: Vec::new(),
        }
    }

    fn observe(&mut self, query_id: &str, sequence: u64) -> Result<()> {
        if sequence > self.last_observed {
            let next = self.last_observed + 1;
            if sequence > next {
                self.missing.push(next..sequence);
            }
            self.last_observed = sequence;
        } else {
            // Retain only gaps, not every delivered sequence: old checkpoint
            // replays and previously observed duplicates must remain valid.
            let index = self.missing.partition_point(|range| range.end <= sequence);
            if self
                .missing
                .get(index)
                .is_some_and(|range| range.contains(&sequence))
            {
                return Err(QueryResultSequenceInversion {
                    query_id: query_id.to_string(),
                    received: sequence,
                    last_observed: self.last_observed,
                }
                .into());
            }
        }
        Ok(())
    }
}

/// Parameters for creating a ReactionBase instance.
///
/// This struct contains only the information that ReactionBase needs to function.
/// Plugin-specific configuration should remain in the plugin crate.
///
/// # Example
///
/// ```ignore
/// use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
///
/// let params = ReactionBaseParams::new("my-reaction", vec!["query1".to_string()])
///     .with_priority_queue_capacity(5000)
///     .with_auto_start(true);
///
/// let base = ReactionBase::new(params);
/// ```
#[derive(Debug, Clone)]
pub struct ReactionBaseParams {
    /// Unique identifier for the reaction
    pub id: String,
    /// List of query IDs this reaction subscribes to
    pub queries: Vec<String>,
    /// Priority queue capacity - defaults to 10000
    pub priority_queue_capacity: Option<usize>,
    /// Whether this reaction should auto-start - defaults to true
    pub auto_start: bool,
    /// Optional recovery policy override (takes precedence over the trait default).
    pub recovery_policy: Option<ReactionRecoveryPolicy>,
}

impl ReactionBaseParams {
    /// Create new params with ID and queries, using defaults for everything else
    pub fn new(id: impl Into<String>, queries: Vec<String>) -> Self {
        Self {
            id: id.into(),
            queries,
            priority_queue_capacity: None,
            auto_start: true, // Default to true like queries
            recovery_policy: None,
        }
    }

    /// Set the priority queue capacity
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether this reaction should auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the recovery policy override for this reaction instance.
    pub fn with_recovery_policy(mut self, policy: ReactionRecoveryPolicy) -> Self {
        self.recovery_policy = Some(policy);
        self
    }
}

/// Base implementation for common reaction functionality
pub struct ReactionBase {
    /// Reaction identifier
    pub id: String,
    /// List of query IDs to subscribe to
    pub queries: Vec<String>,
    /// Whether this reaction should auto-start
    pub auto_start: bool,
    /// Optional recovery policy override
    pub recovery_policy: Option<ReactionRecoveryPolicy>,
    /// Component status handle — always available, wired to graph during initialize().
    status_handle: ComponentStatusHandle,
    /// Runtime context (set by initialize())
    context: Arc<RwLock<Option<ReactionRuntimeContext>>>,
    /// State store provider (extracted from context for convenience)
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    /// Priority queue for result processing
    pub priority_queue: PriorityQueue<QueryResult>,
    /// Per-query effective timestamps; never written into QueryResult payloads.
    result_ordering: Arc<Mutex<HashMap<String, chrono::DateTime<chrono::Utc>>>>,
    /// Handles to subscription forwarder tasks
    pub subscription_tasks: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
    /// Handle to the main processing task
    pub processing_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Sender for shutdown signal to processing task
    pub shutdown_tx: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
    /// Optional identity provider for credential management.
    /// Set either programmatically (via `set_identity_provider`) or automatically
    /// from the runtime context during `initialize()`.
    identity_provider: Arc<RwLock<Option<Arc<dyn IdentityProvider>>>>,
    /// Original raw config JSON from the descriptor, preserving ConfigValue
    /// envelopes (secrets, env vars) for lossless persistence roundtrips.
    raw_config: Option<serde_json::Value>,
}

impl ReactionBase {
    /// Create a new ReactionBase with the given parameters
    ///
    /// Dependencies (query subscriber, state store, graph) are not required during
    /// construction - they will be provided via `initialize()` when the reaction is added to DrasiLib.
    pub fn new(params: ReactionBaseParams) -> Self {
        Self {
            priority_queue: PriorityQueue::new(params.priority_queue_capacity.unwrap_or(10000)),
            result_ordering: Arc::new(Mutex::new(HashMap::new())),
            id: params.id.clone(),
            queries: params.queries,
            auto_start: params.auto_start,
            recovery_policy: params.recovery_policy,
            status_handle: ComponentStatusHandle::new(&params.id),
            context: Arc::new(RwLock::new(None)), // Set by initialize()
            state_store: Arc::new(RwLock::new(None)), // Extracted from context
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            processing_task: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
            identity_provider: Arc::new(RwLock::new(None)),
            raw_config: None,
        }
    }

    /// Initialize the reaction with runtime context.
    ///
    /// This method is called automatically by DrasiLib's `add_reaction()` method.
    /// Plugin developers do not need to call this directly.
    ///
    /// The context provides access to:
    /// - `reaction_id`: The reaction's unique identifier
    /// - `state_store`: Optional persistent state storage
    /// - `update_tx`: mpsc sender for fire-and-forget status updates to the graph
    pub async fn initialize(&self, context: ReactionRuntimeContext) {
        // Store context for later use
        *self.context.write().await = Some(context.clone());

        // Wire the status handle to the graph update channel
        self.status_handle.wire(context.update_tx.clone()).await;

        if let Some(state_store) = context.state_store.as_ref() {
            *self.state_store.write().await = Some(state_store.clone());
        }

        // Store identity provider from context if not already set programmatically
        if let Some(ip) = context.identity_provider.as_ref() {
            let mut guard = self.identity_provider.write().await;
            if guard.is_none() {
                *guard = Some(ip.clone());
            }
        }
    }

    /// Get the runtime context if initialized.
    ///
    /// Returns `None` if `initialize()` has not been called yet.
    pub async fn context(&self) -> Option<ReactionRuntimeContext> {
        self.context.read().await.clone()
    }

    /// Get the state store if configured.
    ///
    /// Returns `None` if no state store was provided in the context.
    pub async fn state_store(&self) -> Option<Arc<dyn StateStoreProvider>> {
        self.state_store.read().await.clone()
    }

    /// Get the identity provider if set.
    ///
    /// Returns the identity provider set either programmatically via
    /// `set_identity_provider()` or from the runtime context during `initialize()`.
    /// Programmatically-set providers take precedence over context providers.
    pub async fn identity_provider(&self) -> Option<Arc<dyn IdentityProvider>> {
        self.identity_provider.read().await.clone()
    }

    /// Set the identity provider programmatically.
    ///
    /// This is typically called during reaction construction when the provider
    /// is available from configuration (e.g., `with_identity_provider()` builder).
    /// Providers set this way take precedence over context-injected providers.
    pub async fn set_identity_provider(&self, provider: Arc<dyn IdentityProvider>) {
        *self.identity_provider.write().await = Some(provider);
    }

    /// Get whether this reaction should auto-start
    pub fn get_auto_start(&self) -> bool {
        self.auto_start
    }

    /// Set the original raw config JSON for lossless persistence roundtrips.
    pub fn set_raw_config(&mut self, config: serde_json::Value) {
        self.raw_config = Some(config);
    }

    /// Get the original raw config JSON, if set by a descriptor.
    pub fn raw_config(&self) -> Option<&serde_json::Value> {
        self.raw_config.as_ref()
    }

    /// Build the properties map for this reaction.
    ///
    /// If `raw_config` was set (descriptor path), returns its top-level keys.
    /// Otherwise, serializes `fallback_dto` (the DTO reconstructed from typed
    /// config) to produce camelCase output.
    ///
    /// This eliminates the duplicated if-let + serialize pattern from plugins.
    pub fn properties_or_serialize<D: serde::Serialize>(
        &self,
        fallback_dto: &D,
    ) -> std::collections::HashMap<String, serde_json::Value> {
        if let Some(serde_json::Value::Object(map)) = self.raw_config.as_ref() {
            return map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        }

        match serde_json::to_value(fallback_dto) {
            Ok(serde_json::Value::Object(map)) => map.into_iter().collect(),
            _ => std::collections::HashMap::new(),
        }
    }

    /// Clone the ReactionBase with shared Arc references
    ///
    /// This creates a new ReactionBase that shares the same underlying
    /// data through Arc references. Useful for passing to spawned tasks.
    pub fn clone_shared(&self) -> Self {
        Self {
            id: self.id.clone(),
            queries: self.queries.clone(),
            auto_start: self.auto_start,
            recovery_policy: self.recovery_policy,
            status_handle: self.status_handle.clone(),
            context: self.context.clone(),
            state_store: self.state_store.clone(),
            priority_queue: self.priority_queue.clone(),
            result_ordering: self.result_ordering.clone(),
            subscription_tasks: self.subscription_tasks.clone(),
            processing_task: self.processing_task.clone(),
            shutdown_tx: self.shutdown_tx.clone(),
            identity_provider: self.identity_provider.clone(),
            raw_config: self.raw_config.clone(),
        }
    }

    /// Create a shutdown channel and store the sender
    ///
    /// Returns the receiver which should be passed to the processing task.
    /// The sender is stored internally and will be triggered by `stop_common()`.
    ///
    /// Call this once per start, before spawning the processing task. A previous
    /// processing generation is stopped and its pending results are discarded.
    pub async fn create_shutdown_channel(&self) -> tokio::sync::oneshot::Receiver<()> {
        let mut shutdown_tx = self.shutdown_tx.write().await;
        let previous_generation = self.processing_task.read().await.is_some()
            || shutdown_tx.as_ref().is_some_and(|tx| tx.is_closed());
        if previous_generation {
            self.priority_queue.close().await;
            if let Some(tx) = shutdown_tx.take() {
                let _ = tx.send(());
            }
            if let Some(task) = self.processing_task.write().await.take() {
                task.abort();
                if let Err(e) = task.await {
                    debug!("[{}] Previous processing task ended: {}", self.id, e);
                }
            }
            self.drain_pending_results().await;
        }
        self.priority_queue.reopen().await;

        let (tx, rx) = tokio::sync::oneshot::channel();
        *shutdown_tx = Some(tx);
        rx
    }

    /// Get the reaction ID
    pub fn get_id(&self) -> &str {
        &self.id
    }

    /// Get the query IDs
    pub fn get_queries(&self) -> &[String] {
        &self.queries
    }

    /// Get current status.
    pub async fn get_status(&self) -> ComponentStatus {
        self.status_handle.get_status().await
    }

    /// Returns a cloneable [`ComponentStatusHandle`] for use in spawned tasks.
    ///
    /// The handle can both read and write the component's status and automatically
    /// notifies the graph on every status change (after `initialize()`).
    pub fn status_handle(&self) -> ComponentStatusHandle {
        self.status_handle.clone()
    }

    /// Set the component's status — updates local state AND notifies the graph.
    ///
    /// This is the single canonical way to change a reaction's status.
    pub async fn set_status(&self, status: ComponentStatus, message: Option<String>) {
        self.status_handle.set_status(status, message).await;
    }

    /// Enqueue a query result for processing.
    ///
    /// The host calls this to forward query results to the reaction's priority queue.
    /// Each query retains its incoming sequence order. Across queries, effective
    /// timestamps are merged, with insertion order breaking ties.
    pub async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        // Capture the generation before waiting for the ordering lock so a
        // sender cannot carry an old result across a stop/restart boundary.
        let generation = self.priority_queue.generation();
        let mut ordering = self.result_ordering.lock().await;
        let timestamp = result.timestamp;
        let query_id = result.query_id.clone();
        let ordering_timestamp = ordering
            .get(&query_id)
            .map_or(timestamp, |last| (*last).max(timestamp));
        self.priority_queue
            .enqueue_wait_with_ordering_timestamp(Arc::new(result), ordering_timestamp, generation)
            .await?;
        ordering.insert(query_id, ordering_timestamp);
        Ok(())
    }

    // ========================================================================
    // Checkpoint helpers
    // ========================================================================

    /// Read the persisted checkpoint for a single query subscription.
    ///
    /// Returns `Ok(None)` if no checkpoint exists (fresh start).
    /// Errors propagate from the state store or deserialization.
    pub async fn read_checkpoint(&self, query_id: &str) -> Result<Option<ReactionCheckpoint>> {
        let store = self.state_store.read().await;
        match store.as_ref() {
            Some(s) => {
                crate::reactions::checkpoint::read_checkpoint(s.as_ref(), &self.id, query_id).await
            }
            None => Ok(None),
        }
    }

    /// Read all persisted checkpoints for every query this reaction subscribes to.
    ///
    /// Returns a map from query ID to checkpoint. Missing checkpoints (fresh
    /// subscriptions) are simply omitted from the result.
    pub async fn read_all_checkpoints(
        &self,
    ) -> Result<std::collections::HashMap<String, ReactionCheckpoint>> {
        let store = self.state_store.read().await;
        match store.as_ref() {
            Some(s) => {
                crate::reactions::checkpoint::read_checkpoints_batch(
                    s.as_ref(),
                    &self.id,
                    &self.queries,
                )
                .await
            }
            None => Ok(std::collections::HashMap::new()),
        }
    }

    /// Persist a checkpoint for a single query subscription.
    ///
    /// This atomically writes the checkpoint to the state store. It should be
    /// called after a batch of query results has been successfully processed.
    pub async fn write_checkpoint(
        &self,
        query_id: &str,
        checkpoint: &ReactionCheckpoint,
    ) -> Result<()> {
        let store = self.state_store.read().await;
        let store = store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("No state store configured — cannot write checkpoint")
        })?;
        crate::reactions::checkpoint::write_checkpoint(
            store.as_ref(),
            &self.id,
            query_id,
            checkpoint,
        )
        .await
    }

    /// Perform common cleanup operations
    ///
    /// This method handles:
    /// 1. Sending shutdown signal to processing task (for graceful termination)
    /// 2. Aborting all subscription forwarder tasks
    /// 3. Waiting for or aborting the processing task
    /// 4. Draining the priority queue
    pub async fn stop_common(&self) -> Result<()> {
        info!("Stopping reaction: {}", self.id);

        // A producer can hold result_ordering while waiting for queue capacity.
        self.priority_queue.close().await;

        // Send shutdown signal to processing task (if it's using tokio::select!)
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }

        // Abort all subscription forwarder tasks
        let mut subscription_tasks = self.subscription_tasks.write().await;
        for task in subscription_tasks.drain(..) {
            task.abort();
        }
        drop(subscription_tasks);

        // Wait for the processing task to complete (with timeout), or abort it
        let mut processing_task = self.processing_task.write().await;
        if let Some(mut task) = processing_task.take() {
            // Give the task a short time to respond to the shutdown signal
            match tokio::time::timeout(std::time::Duration::from_secs(2), &mut task).await {
                Ok(Ok(())) => {
                    debug!("[{}] Processing task completed gracefully", self.id);
                }
                Ok(Err(e)) => {
                    // Task was aborted or panicked
                    debug!("[{}] Processing task ended: {}", self.id, e);
                }
                Err(_) => {
                    // Timeout - task didn't respond to shutdown signal
                    warn!(
                        "[{}] Processing task did not respond to shutdown signal within timeout, aborting",
                        self.id
                    );
                    task.abort();
                    let _ = task.await;
                }
            }
        }
        drop(processing_task);

        self.drain_pending_results().await;

        self.set_status(
            ComponentStatus::Stopped,
            Some(format!("Reaction '{}' stopped", self.id)),
        )
        .await;
        info!("Reaction '{}' stopped", self.id);

        Ok(())
    }

    async fn drain_pending_results(&self) {
        let mut ordering = self.result_ordering.lock().await;
        let drained_events = self.priority_queue.drain().await;
        if !drained_events.is_empty() {
            info!(
                "[{}] Drained {} pending events from priority queue",
                self.id,
                drained_events.len()
            );
        }
        ordering.clear();
    }

    /// Clear the reaction's state store partition.
    ///
    /// This is called during deprovision to remove all persisted state
    /// associated with this reaction. Reactions that override `deprovision()`
    /// can call this to clean up their state store.
    pub async fn deprovision_common(&self) -> Result<()> {
        info!("Deprovisioning reaction '{}'", self.id);
        if let Some(store) = self.state_store().await {
            let count = store.clear_store(&self.id).await.map_err(|e| {
                anyhow::anyhow!(
                    "Failed to clear state store for reaction '{}': {}",
                    self.id,
                    e
                )
            })?;
            info!(
                "Cleared {} keys from state store for reaction '{}'",
                count, self.id
            );
        }
        Ok(())
    }

    /// Set the processing task handle
    pub async fn set_processing_task(&self, task: tokio::task::JoinHandle<()>) {
        *self.processing_task.write().await = Some(task);
    }

    /// Run a standard dequeue → dedup → handler → checkpoint loop.
    ///
    /// This is an optional convenience for reactions that follow the common
    /// pattern.  Reactions needing custom scheduling, batching, or
    /// multi-query ordering should implement their own loop.
    ///
    /// The loop:
    /// 1. Dequeues from the priority queue (blocks until available).
    /// 2. Checks the event's sequence against the persisted checkpoint —
    ///    events at or before the checkpoint are silently skipped (dedup).
    /// 3. Calls `handler` with the event.
    /// 4. On success, writes a new checkpoint with the event's sequence,
    ///    preserving the `config_hash` from the initial checkpoint map
    ///    (or 0 if no prior checkpoint exists for that query).
    /// 5. Breaks when `shutdown_rx` fires.
    ///
    /// Previously unseen sequence regressions close the queue generation, wake
    /// blocked producers, publish `Error`, and return the inversion error rather
    /// than treating it as a replay duplicate. This fences ingress but does not
    /// abort host-owned subscription tasks. Ordered gaps remain valid.
    ///
    /// # Arguments
    /// * `shutdown_rx` — receiver created via [`create_shutdown_channel`].
    /// * `initial_checkpoints` — pre-loaded checkpoint map (from bootstrap
    ///   orchestration). The loop uses these for dedup and preserves each
    ///   query's `config_hash` when advancing the sequence.
    /// * `handler` — async function receiving a [`QueryResult`].  Return
    ///   `Ok(())` to advance the checkpoint, or `Err` to leave it unchanged
    ///   (the event will NOT be retried automatically).
    pub async fn run_standard_loop<F, Fut>(
        &self,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
        initial_checkpoints: std::collections::HashMap<String, ReactionCheckpoint>,
        handler: F,
    ) -> Result<()>
    where
        F: Fn(Arc<crate::channels::QueryResult>) -> Fut + Send + Sync,
        Fut: std::future::Future<Output = Result<()>> + Send,
    {
        let mut checkpoints = initial_checkpoints;
        let mut sequences = HashMap::new();

        loop {
            let event = tokio::select! {
                biased;
                _ = &mut shutdown_rx => {
                    break;
                }
                event = self.priority_queue.dequeue() => event,
            };

            let query_id = &event.query_id;
            let seq = event.sequence;

            if let Err(e) = sequences
                .entry(query_id.clone())
                .or_insert_with(|| {
                    QuerySequenceCursor::new(checkpoints.get(query_id).map_or(0, |cp| cp.sequence))
                })
                .observe(query_id, seq)
            {
                // Fence producers before reporting failure; do not join this
                // processing task from itself through stop_common().
                self.priority_queue.close().await;
                error!("[{}] Stopping result processing: {e}", self.id);
                self.set_status(
                    ComponentStatus::Error,
                    Some(format!("Result processing stopped: {e}")),
                )
                .await;
                return Err(e);
            }

            // Dedup: skip events at or before the checkpoint.
            if let Some(cp) = checkpoints.get(query_id) {
                if seq <= cp.sequence {
                    debug!(
                        "[{}] Skipping already-processed event: query={}, seq={} (checkpoint={})",
                        self.id, query_id, seq, cp.sequence
                    );
                    continue;
                }
            }

            // Invoke the user-provided handler.
            if let Err(e) = handler(Arc::clone(&event)).await {
                error!(
                    "[{}] Handler error for query={}, seq={}: {:#}",
                    self.id, query_id, seq, e
                );
                // Don't advance the checkpoint — the event was not
                // successfully processed.
                continue;
            }

            // Advance the checkpoint, preserving the config_hash from bootstrap.
            let config_hash = checkpoints
                .get(query_id)
                .map(|cp| cp.config_hash)
                .unwrap_or(0);
            let cp = ReactionCheckpoint {
                sequence: seq,
                config_hash,
            };
            self.write_checkpoint(query_id, &cp).await?;
            checkpoints.insert(query_id.clone(), cp);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn query_result(query_id: &str, sequence: u64, timestamp: i64) -> QueryResult {
        QueryResult::new(
            query_id.to_string(),
            sequence,
            chrono::DateTime::from_timestamp_millis(timestamp).unwrap(),
            vec![],
            Default::default(),
        )
    }

    async fn store_backed_base(id: &str) -> ReactionBase {
        let (graph, _rx) = crate::component_graph::ComponentGraph::new("test-instance");
        let base = ReactionBase::new(ReactionBaseParams::new(
            id,
            vec!["q1".to_string(), "q2".to_string(), "q3".to_string()],
        ));
        base.initialize(ReactionRuntimeContext::new(
            "test-instance",
            id,
            Some(Arc::new(crate::state_store::MemoryStateStoreProvider::new())),
            graph.update_sender(),
            None,
        ))
        .await;
        base
    }

    async fn collect_queued_results(
        base: &ReactionBase,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
        checkpoints: HashMap<String, ReactionCheckpoint>,
    ) -> Vec<Arc<QueryResult>> {
        let expected_dequeued = base.priority_queue.metrics().await.total_dequeued
            + base.priority_queue.depth().await as u64;
        let processed = Arc::new(Mutex::new(Vec::new()));
        let processed_clone = processed.clone();
        let base_clone = base.clone_shared();
        let task = tokio::spawn(async move {
            base_clone
                .run_standard_loop(shutdown_rx, checkpoints, |event| {
                    let processed = processed_clone.clone();
                    async move {
                        processed.lock().await.push(event);
                        Ok(())
                    }
                })
                .await
        });
        finish_queued_loop(base, expected_dequeued, task).await;
        let results = processed.lock().await.clone();
        results
    }

    async fn finish_queued_loop(
        base: &ReactionBase,
        expected_dequeued: u64,
        mut task: tokio::task::JoinHandle<Result<()>>,
    ) {
        // Dequeue completion includes skipped replay inputs. Joining the actual
        // loop also waits for the final handler and checkpoint, not just its pop.
        let completed = tokio::time::timeout(Duration::from_secs(30), async {
            while base.priority_queue.metrics().await.total_dequeued < expected_dequeued
                && !task.is_finished()
            {
                tokio::task::yield_now().await;
            }
            base.stop_common().await.unwrap();
            (&mut task).await.unwrap().unwrap();
        })
        .await;
        if completed.is_err() {
            task.abort();
            let _ = task.await;
            panic!(
                "queue/handler/checkpoint completion timed out: expected {expected_dequeued} \
                 dequeued inputs, metrics={:?}",
                base.priority_queue.metrics().await
            );
        }
    }

    #[tokio::test]
    async fn test_reaction_base_creation() {
        let params = ReactionBaseParams::new("test-reaction", vec!["query1".to_string()])
            .with_priority_queue_capacity(5000);

        let base = ReactionBase::new(params);
        assert_eq!(base.id, "test-reaction");
        assert_eq!(base.get_status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_status_transitions() {
        use crate::context::ReactionRuntimeContext;

        let (graph, _rx) = crate::component_graph::ComponentGraph::new("test-instance");
        let update_tx = graph.update_sender();
        let graph = Arc::new(RwLock::new(graph));
        let params = ReactionBaseParams::new("test-reaction", vec![]);

        let base = ReactionBase::new(params);

        // Create context and initialize
        let context =
            ReactionRuntimeContext::new("test-instance", "test-reaction", None, update_tx, None);
        base.initialize(context).await;

        // Test status transition
        base.set_status(ComponentStatus::Starting, Some("Starting test".to_string()))
            .await;

        assert_eq!(base.get_status().await, ComponentStatus::Starting);

        // Check event was sent via graph broadcast
        let mut event_rx = graph.read().await.subscribe();
        // The status was already set; emit another event to verify the graph path works
        base.set_status(ComponentStatus::Running, Some("Running test".to_string()))
            .await;

        assert_eq!(base.get_status().await, ComponentStatus::Running);
    }

    #[tokio::test]
    async fn test_priority_queue_operations() {
        let params =
            ReactionBaseParams::new("test-reaction", vec![]).with_priority_queue_capacity(10);

        let base = ReactionBase::new(params);

        // Create a test query result
        let query_result = QueryResult::new(
            "test-query".to_string(),
            0,
            chrono::Utc::now(),
            vec![],
            Default::default(),
        );

        // Enqueue result
        let enqueued = base.priority_queue.enqueue(Arc::new(query_result)).await;
        assert!(enqueued);

        // Drain queue
        let drained = base.priority_queue.drain().await;
        assert_eq!(drained.len(), 1);
    }

    #[tokio::test]
    async fn test_multi_query_order_preserves_query_result_bytes() {
        let base = store_backed_base("multi-query").await;
        let shutdown_rx = base.create_shutdown_channel().await;
        let results = [
            query_result("q1", 6, 30),
            query_result("q2", 1, 20),
            query_result("q1", 7, 10),
            query_result("q3", 1, 30),
            query_result("q2", 2, 30),
        ];
        let wire_bytes: Vec<_> = results
            .iter()
            .map(|result| rmp_serde::to_vec_named(result).unwrap())
            .collect();
        let persisted_bytes: Vec<_> = results
            .iter()
            .map(|result| bincode::serialize(result).unwrap())
            .collect();
        let shared = base.clone_shared();
        for (index, result) in results.into_iter().enumerate() {
            let producer = if index % 2 == 0 { &base } else { &shared };
            producer.enqueue_query_result(result).await.unwrap();
        }

        let processed = collect_queued_results(&base, shutdown_rx, HashMap::new()).await;
        assert_eq!(processed.len(), 5);
        for (result, index) in processed.iter().zip([1, 0, 2, 3, 4]) {
            assert_eq!(
                rmp_serde::to_vec_named(result.as_ref()).unwrap(),
                wire_bytes[index]
            );
            assert_eq!(
                bincode::serialize(result.as_ref()).unwrap(),
                persisted_bytes[index]
            );
        }
        assert_eq!(
            base.read_checkpoint("q1").await.unwrap().unwrap().sequence,
            7
        );
        assert_eq!(
            base.read_checkpoint("q2").await.unwrap().unwrap().sequence,
            2
        );
    }

    #[tokio::test]
    async fn test_replays_below_checkpoint_and_earlier_deliveries_are_still_skipped() {
        let base = store_backed_base("replay").await;
        let shutdown_rx = base.create_shutdown_channel().await;
        let checkpoints = HashMap::from([(
            "q1".to_string(),
            ReactionCheckpoint {
                sequence: 5,
                config_hash: 42,
            },
        )]);
        for sequence in [5, 3, 6, 7, 6, 4, 7, 8] {
            base.enqueue_query_result(query_result("q1", sequence, 100))
                .await
                .unwrap();
        }
        let processed = collect_queued_results(&base, shutdown_rx, checkpoints).await;
        assert_eq!(
            processed
                .iter()
                .map(|result| result.sequence)
                .collect::<Vec<_>>(),
            vec![6, 7, 8]
        );
        assert_eq!(
            base.read_checkpoint("q1").await.unwrap().unwrap(),
            ReactionCheckpoint {
                sequence: 8,
                config_hash: 42,
            }
        );
    }

    #[tokio::test]
    async fn test_unseen_inversion_fences_running_generation_and_allows_cleanup_restart() {
        use crate::channels::priority_queue::PriorityQueueClosed;
        use crate::component_graph::{ComponentGraph, ComponentUpdate};

        let (mut graph, mut updates) = ComponentGraph::new("test-instance");
        graph.register_query("q1", HashMap::new(), &[]).unwrap();
        graph
            .register_reaction("inversion", HashMap::new(), &["q1".to_string()])
            .unwrap();
        let base = ReactionBase::new(
            ReactionBaseParams::new("inversion", vec!["q1".to_string()])
                .with_priority_queue_capacity(2),
        );
        base.initialize(ReactionRuntimeContext::new(
            "test-instance",
            "inversion",
            Some(Arc::new(crate::state_store::MemoryStateStoreProvider::new())),
            graph.update_sender(),
            None,
        ))
        .await;
        for status in [ComponentStatus::Starting, ComponentStatus::Running] {
            base.set_status(status, None).await;
            graph.apply_update(updates.recv().await.unwrap());
        }
        assert_eq!(base.get_status().await, ComponentStatus::Running);
        assert_eq!(
            graph.get_component("inversion").unwrap().status,
            ComponentStatus::Running
        );

        base.write_checkpoint(
            "q1",
            &ReactionCheckpoint {
                sequence: 5,
                config_hash: 42,
            },
        )
        .await
        .unwrap();
        let checkpoints = base.read_all_checkpoints().await.unwrap();
        let shutdown_rx = base.create_shutdown_channel().await;
        for (sequence, timestamp) in [(7, 10), (6, 20)] {
            base.enqueue_query_result(query_result("q1", sequence, timestamp))
                .await
                .unwrap();
        }

        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let processed = Arc::new(Mutex::new(Vec::new()));
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let loop_base = base.clone_shared();
        let loop_entered = entered.clone();
        let loop_release = release.clone();
        let loop_processed = processed.clone();
        base.set_processing_task(tokio::spawn(async move {
            let result = loop_base
                .run_standard_loop(shutdown_rx, checkpoints, |event| {
                    let entered = loop_entered.clone();
                    let release = loop_release.clone();
                    let processed = loop_processed.clone();
                    async move {
                        processed.lock().await.push(event.sequence);
                        entered.notify_one();
                        release.notified().await;
                        Ok(())
                    }
                })
                .await;
            result_tx.send(result).unwrap();
        }))
        .await;

        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        base.enqueue_query_result(query_result("q1", 8, 30))
            .await
            .unwrap();
        // Poll both producers to Pending while the handler is gated: one owns
        // the ordering lock and waits for capacity, the other waits for that lock.
        let blocked = base.enqueue_query_result(query_result("q1", 9, 40));
        tokio::pin!(blocked);
        assert!(futures::poll!(blocked.as_mut()).is_pending());
        let waiting = base.enqueue_query_result(query_result("q1", 10, 50));
        tokio::pin!(waiting);
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        assert_eq!(base.priority_queue.depth().await, 2);
        release.notify_one();

        let error = tokio::time::timeout(Duration::from_secs(2), result_rx)
            .await
            .expect("the registered consumer must fail without trying to join itself")
            .unwrap()
            .unwrap_err();
        let inversion = error
            .downcast_ref::<QueryResultSequenceInversion>()
            .unwrap();
        assert_eq!(inversion.query_id, "q1");
        assert_eq!(inversion.received, 6);
        assert_eq!(inversion.last_observed, 7);
        assert_eq!(base.get_status().await, ComponentStatus::Error);
        let update = tokio::time::timeout(Duration::from_secs(2), updates.recv())
            .await
            .unwrap()
            .unwrap();
        let ComponentUpdate::Status {
            status, message, ..
        } = &update;
        assert_eq!(*status, ComponentStatus::Error);
        assert!(message.as_ref().unwrap().contains(&error.to_string()));
        graph.apply_update(update);
        assert_eq!(
            graph.get_component("inversion").unwrap().status,
            ComponentStatus::Error
        );
        let (blocked, waiting) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(blocked, waiting)
        })
        .await
        .expect("an inversion must fence capacity and ordering waiters");
        for result in [blocked, waiting] {
            assert!(result.unwrap_err().is::<PriorityQueueClosed>());
        }
        assert!(base
            .enqueue_query_result(query_result("q1", 11, 60))
            .await
            .unwrap_err()
            .is::<PriorityQueueClosed>());
        assert_eq!(*processed.lock().await, vec![7]);
        assert_eq!(
            base.read_checkpoint("q1").await.unwrap().unwrap(),
            ReactionCheckpoint {
                sequence: 7,
                config_hash: 42
            }
        );
        assert_eq!(base.priority_queue.depth().await, 1);

        tokio::time::timeout(Duration::from_secs(2), base.stop_common())
            .await
            .unwrap()
            .unwrap();
        assert!(base.processing_task.read().await.is_none());
        assert!(base.priority_queue.is_empty().await);
        assert!(base.result_ordering.lock().await.is_empty());
        let shutdown_rx = base.create_shutdown_channel().await;
        base.set_status(ComponentStatus::Starting, None).await;
        base.set_status(ComponentStatus::Running, None).await;
        let checkpoints = base.read_all_checkpoints().await.unwrap();
        base.enqueue_query_result(query_result("q1", 7, 1))
            .await
            .unwrap();
        base.enqueue_query_result(query_result("q1", 8, 0))
            .await
            .unwrap();
        let restarted = collect_queued_results(&base, shutdown_rx, checkpoints).await;
        assert_eq!(
            restarted
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![8]
        );
        assert_eq!(
            base.read_checkpoint("q1").await.unwrap().unwrap(),
            ReactionCheckpoint {
                sequence: 8,
                config_hash: 42
            }
        );
    }

    #[test]
    fn test_sequence_cursor_accepts_gaps_replays_and_u64_max() {
        let mut cursor = QuerySequenceCursor::new(5);
        for sequence in [3, 5, 7, 9, 7, 3, u64::MAX, 9, u64::MAX] {
            cursor.observe("q1", sequence).unwrap();
        }
        for sequence in [6, 8, u64::MAX - 1] {
            assert!(cursor
                .observe("q1", sequence)
                .unwrap_err()
                .is::<QueryResultSequenceInversion>());
        }
    }

    #[test]
    fn test_sequence_cursor_retains_one_range_per_gap_not_per_missing_sequence() {
        let mut cursor = QuerySequenceCursor::new(0);
        for sequence in 0..=100_000 {
            cursor.observe("q1", sequence).unwrap();
        }
        assert_eq!(cursor.missing.capacity(), 0);

        for sequence in (100_002..=108_192).step_by(2) {
            cursor.observe("q1", sequence).unwrap();
        }
        assert_eq!(cursor.missing.len(), 4_096);
        cursor.observe("q1", u64::MAX).unwrap();
        assert_eq!(cursor.missing.len(), 4_097);
        assert_eq!(cursor.missing.last().unwrap(), &(108_193..u64::MAX));
        for sequence in [0, 100_000, 100_002, 108_192, u64::MAX] {
            cursor.observe("q1", sequence).unwrap();
        }
        assert_eq!(cursor.missing.len(), 4_097);
        for sequence in [100_001, 108_191, u64::MAX - 1] {
            assert!(cursor.observe("q1", sequence).is_err());
        }

        let mut restarted = QuerySequenceCursor::new(u64::MAX);
        for sequence in [0, 1, u64::MAX - 1, u64::MAX] {
            restarted.observe("q1", sequence).unwrap();
        }
        assert_eq!(restarted.missing.capacity(), 0);
    }

    #[tokio::test]
    async fn test_stop_wakes_capacity_and_ordering_waiters() {
        let base = ReactionBase::new(
            ReactionBaseParams::new("full-queue", vec!["q1".to_string()])
                .with_priority_queue_capacity(1),
        );
        let _shutdown_rx = base.create_shutdown_channel().await;
        base.enqueue_query_result(query_result("q1", 100, 100))
            .await
            .unwrap();
        let blocked = base.enqueue_query_result(query_result("q1", 101, 101));
        tokio::pin!(blocked);
        assert!(futures::poll!(blocked.as_mut()).is_pending());
        let waiting = base.enqueue_query_result(query_result("q2", 1, 1));
        tokio::pin!(waiting);
        assert!(futures::poll!(waiting.as_mut()).is_pending());

        let (stopped, blocked, waiting) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(base.stop_common(), blocked, waiting)
        })
        .await
        .expect("stop must close the queue before acquiring the ordering lock");
        stopped.unwrap();
        for result in [blocked, waiting] {
            assert!(result
                .unwrap_err()
                .is::<crate::channels::priority_queue::PriorityQueueClosed>());
        }
        assert!(base.priority_queue.is_empty().await);
        assert!(base.result_ordering.lock().await.is_empty());
        assert!(base
            .enqueue_query_result(query_result("q1", 102, 102))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_restarting_a_full_failed_generation_wakes_old_senders() {
        let base = ReactionBase::new(
            ReactionBaseParams::new("full-restart", vec![]).with_priority_queue_capacity(1),
        );
        let shutdown = base.create_shutdown_channel().await;
        let consumer = tokio::spawn(async move {
            let _ = shutdown.await;
        });
        let old_consumer = consumer.abort_handle();
        base.set_processing_task(consumer).await;
        base.enqueue_query_result(query_result("q1", 100, 100))
            .await
            .unwrap();
        let blocked = base.enqueue_query_result(query_result("q1", 101, 101));
        tokio::pin!(blocked);
        assert!(futures::poll!(blocked.as_mut()).is_pending());
        let waiting = base.enqueue_query_result(query_result("q1", 102, 102));
        tokio::pin!(waiting);
        assert!(futures::poll!(waiting.as_mut()).is_pending());

        let (_new_shutdown, blocked, waiting) =
            tokio::time::timeout(Duration::from_secs(1), async {
                tokio::join!(base.create_shutdown_channel(), blocked, waiting)
            })
            .await
            .expect("restart must close the old generation before waiting for its senders");
        for result in [blocked, waiting] {
            assert!(result
                .unwrap_err()
                .is::<crate::channels::priority_queue::PriorityQueueClosed>());
        }
        assert!(old_consumer.is_finished());
        assert!(base.priority_queue.is_empty().await);
        assert!(base.result_ordering.lock().await.is_empty());
        base.enqueue_query_result(query_result("q1", 1, 10))
            .await
            .unwrap();
        assert_eq!(base.priority_queue.dequeue().await.sequence, 1);
        base.stop_common().await.unwrap();
    }

    #[tokio::test]
    async fn test_clean_restart_resets_ordering_for_fresh_lower_sequences() {
        let base = ReactionBase::new(ReactionBaseParams::new("restart", vec![]));
        let _first_rx = base.create_shutdown_channel().await;
        base.enqueue_query_result(query_result("q1", 100, 100))
            .await
            .unwrap();
        base.stop_common().await.unwrap();
        let _next_rx = base.create_shutdown_channel().await;
        base.enqueue_query_result(query_result("q2", 1, 20))
            .await
            .unwrap();
        base.enqueue_query_result(query_result("q1", 1, 10))
            .await
            .unwrap();
        let result = base.priority_queue.dequeue().await;
        assert_eq!((result.query_id.as_str(), result.sequence), ("q1", 1));
        assert_eq!(base.priority_queue.dequeue().await.query_id, "q2");
        base.stop_common().await.unwrap();
    }

    #[tokio::test]
    async fn test_failed_generation_discards_stale_results_and_ordering() {
        let base = ReactionBase::new(ReactionBaseParams::new("failed", vec![]));
        let first_rx = base.create_shutdown_channel().await;
        base.enqueue_query_result(query_result("q1", 100, 100))
            .await
            .unwrap();
        let task = tokio::spawn(async move {
            drop(first_rx);
        });
        let aborted = task.abort_handle();
        base.set_processing_task(task).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !aborted.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        base.set_status(ComponentStatus::Error, None).await;

        let _next_rx = base.create_shutdown_channel().await;
        assert!(base.processing_task.read().await.is_none());
        assert!(base.priority_queue.is_empty().await);
        assert!(base.result_ordering.lock().await.is_empty());
        base.enqueue_query_result(query_result("q2", 1, 20))
            .await
            .unwrap();
        base.enqueue_query_result(query_result("q1", 1, 10))
            .await
            .unwrap();
        let result = base.priority_queue.dequeue().await;
        assert_eq!((result.query_id.as_str(), result.sequence), ("q1", 1));
        assert_eq!(base.priority_queue.dequeue().await.query_id, "q2");
        base.stop_common().await.unwrap();
    }

    #[tokio::test]
    async fn test_failed_start_before_task_registration_resets_ordering() {
        let base = ReactionBase::new(ReactionBaseParams::new("failed-start", vec![]));
        let first_rx = base.create_shutdown_channel().await;
        base.enqueue_query_result(query_result("q1", 100, 100))
            .await
            .unwrap();
        drop(first_rx);
        let _next_rx = base.create_shutdown_channel().await;
        assert!(base.priority_queue.is_empty().await);
        assert!(base.result_ordering.lock().await.is_empty());
        base.enqueue_query_result(query_result("q1", 1, 10))
            .await
            .unwrap();
        assert_eq!(base.priority_queue.dequeue().await.sequence, 1);
        base.stop_common().await.unwrap();
    }

    #[tokio::test]
    async fn test_cancelled_enqueue_does_not_advance_ordering() {
        let base = ReactionBase::new(
            ReactionBaseParams::new("cancelled", vec![]).with_priority_queue_capacity(2),
        );
        for sequence in [4, 5] {
            base.enqueue_query_result(query_result("q1", sequence, 10))
                .await
                .unwrap();
        }
        let mut blocked = Box::pin(base.enqueue_query_result(query_result("q1", 6, 100)));
        assert!(futures::poll!(blocked.as_mut()).is_pending());
        drop(blocked);
        for _ in 0..2 {
            base.priority_queue.dequeue().await;
        }
        base.enqueue_query_result(query_result("q1", 6, 20))
            .await
            .unwrap();
        base.enqueue_query_result(query_result("q2", 1, 30))
            .await
            .unwrap();
        assert_eq!(base.priority_queue.dequeue().await.query_id, "q1");
        assert_eq!(base.priority_queue.dequeue().await.query_id, "q2");
    }

    #[tokio::test]
    async fn test_event_without_initialization() {
        // Test that set_status works even without context initialization
        let params = ReactionBaseParams::new("test-reaction", vec![]);

        let base = ReactionBase::new(params);

        // This should succeed without panicking (silently updates local only when handle is None)
        base.set_status(ComponentStatus::Starting, None).await;
    }

    // =============================================================================
    // Shutdown Channel Tests
    // =============================================================================

    #[tokio::test]
    async fn test_create_shutdown_channel() {
        let params = ReactionBaseParams::new("test-reaction", vec![]);
        let base = ReactionBase::new(params);

        // Initially no shutdown_tx
        assert!(base.shutdown_tx.read().await.is_none());

        // Create channel
        let rx = base.create_shutdown_channel().await;

        // Verify tx is stored
        assert!(base.shutdown_tx.read().await.is_some());

        // Verify receiver is valid (dropping it should not panic)
        drop(rx);
    }

    #[tokio::test]
    async fn test_shutdown_channel_signal() {
        let params = ReactionBaseParams::new("test-reaction", vec![]);
        let base = ReactionBase::new(params);

        let mut rx = base.create_shutdown_channel().await;

        // Send signal
        if let Some(tx) = base.shutdown_tx.write().await.take() {
            tx.send(()).unwrap();
        }

        // Verify signal received
        let result = rx.try_recv();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_shutdown_channel_replaced_on_second_create() {
        let params = ReactionBaseParams::new("test-reaction", vec![]);
        let base = ReactionBase::new(params);

        // Create first channel
        let _rx1 = base.create_shutdown_channel().await;
        base.enqueue_query_result(query_result("q1", 6, 30))
            .await
            .unwrap();

        // Create second channel (should replace the first)
        let mut rx2 = base.create_shutdown_channel().await;
        assert_eq!(base.priority_queue.depth().await, 1);
        assert_eq!(
            base.result_ordering.lock().await["q1"].timestamp_millis(),
            30
        );

        // Send signal - should go to second channel
        if let Some(tx) = base.shutdown_tx.write().await.take() {
            tx.send(()).unwrap();
        }

        // Second receiver should get the signal
        let result = rx2.try_recv();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_stop_common_sends_shutdown_signal() {
        let params = ReactionBaseParams::new("test-reaction", vec![]);
        let base = ReactionBase::new(params);

        let mut rx = base.create_shutdown_channel().await;

        // Spawn a task that waits for shutdown
        let shutdown_received = Arc::new(AtomicBool::new(false));
        let shutdown_flag = shutdown_received.clone();

        let task = tokio::spawn(async move {
            tokio::select! {
                _ = &mut rx => {
                    shutdown_flag.store(true, Ordering::SeqCst);
                }
            }
        });

        base.set_processing_task(task).await;

        // Call stop_common - should send shutdown signal and await the task
        let _ = base.stop_common().await;

        // stop_common awaits the processing task, so the flag should already be set
        assert!(
            shutdown_received.load(Ordering::SeqCst),
            "Processing task should have received shutdown signal"
        );
    }

    #[tokio::test]
    async fn test_graceful_shutdown_timing() {
        let params = ReactionBaseParams::new("test-reaction", vec![]);
        let base = ReactionBase::new(params);

        let rx = base.create_shutdown_channel().await;

        // Spawn task that uses select! pattern like real reactions
        let task = tokio::spawn(async move {
            let mut shutdown_rx = rx;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {
                        // Simulates waiting on priority_queue.dequeue()
                    }
                }
            }
        });

        base.set_processing_task(task).await;

        // Measure shutdown time
        let start = std::time::Instant::now();
        let _ = base.stop_common().await;
        let elapsed = start.elapsed();

        // Should complete quickly (< 500ms), not hit 2s timeout
        assert!(
            elapsed < Duration::from_millis(500),
            "Shutdown took {elapsed:?}, expected < 500ms. Task may not be responding to shutdown signal."
        );
    }

    #[tokio::test]
    async fn test_stop_common_without_shutdown_channel() {
        // Test that stop_common works even if no shutdown channel was created
        let params = ReactionBaseParams::new("test-reaction", vec![]);
        let base = ReactionBase::new(params);

        // Don't create shutdown channel - just spawn a short-lived task
        let task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(10)).await;
        });

        base.set_processing_task(task).await;

        // stop_common should still work
        let result = base.stop_common().await;
        assert!(result.is_ok());
    }

    // =============================================================================
    // Accessor Tests
    // =============================================================================

    #[tokio::test]
    async fn test_get_id() {
        let params = ReactionBaseParams::new("my-reaction-42", vec![]);
        let base = ReactionBase::new(params);
        assert_eq!(base.get_id(), "my-reaction-42");
    }

    #[tokio::test]
    async fn test_get_queries() {
        let queries = vec!["query-a".to_string(), "query-b".to_string(), "query-c".to_string()];
        let params = ReactionBaseParams::new("r1", queries.clone());
        let base = ReactionBase::new(params);
        assert_eq!(base.get_queries(), &queries[..]);
    }

    #[tokio::test]
    async fn test_get_queries_empty() {
        let params = ReactionBaseParams::new("r1", vec![]);
        let base = ReactionBase::new(params);
        assert!(base.get_queries().is_empty());
    }

    #[tokio::test]
    async fn test_get_auto_start_default_true() {
        let params = ReactionBaseParams::new("r1", vec![]);
        let base = ReactionBase::new(params);
        assert!(base.get_auto_start());
    }

    #[tokio::test]
    async fn test_get_auto_start_override_false() {
        let params = ReactionBaseParams::new("r1", vec![]).with_auto_start(false);
        let base = ReactionBase::new(params);
        assert!(!base.get_auto_start());
    }

    // =============================================================================
    // Context / State Store / Identity Provider Tests
    // =============================================================================

    #[tokio::test]
    async fn test_context_none_before_initialize() {
        let params = ReactionBaseParams::new("r1", vec![]);
        let base = ReactionBase::new(params);
        assert!(base.context().await.is_none());
    }

    #[tokio::test]
    async fn test_context_some_after_initialize() {
        let (graph, _rx) = crate::component_graph::ComponentGraph::new("inst");
        let update_tx = graph.update_sender();
        let context = ReactionRuntimeContext::new("inst", "r1", None, update_tx, None);

        let params = ReactionBaseParams::new("r1", vec![]);
        let base = ReactionBase::new(params);
        base.initialize(context).await;

        let ctx = base.context().await;
        assert!(ctx.is_some());
        assert_eq!(ctx.unwrap().reaction_id, "r1");
    }

    #[tokio::test]
    async fn test_state_store_none_when_not_configured() {
        let params = ReactionBaseParams::new("r1", vec![]);
        let base = ReactionBase::new(params);
        assert!(base.state_store().await.is_none());
    }

    #[tokio::test]
    async fn test_state_store_none_after_initialize_without_store() {
        let (graph, _rx) = crate::component_graph::ComponentGraph::new("inst");
        let update_tx = graph.update_sender();
        let context = ReactionRuntimeContext::new("inst", "r1", None, update_tx, None);

        let params = ReactionBaseParams::new("r1", vec![]);
        let base = ReactionBase::new(params);
        base.initialize(context).await;

        assert!(base.state_store().await.is_none());
    }

    #[tokio::test]
    async fn test_identity_provider_none_by_default() {
        let params = ReactionBaseParams::new("r1", vec![]);
        let base = ReactionBase::new(params);
        assert!(base.identity_provider().await.is_none());
    }

    // =============================================================================
    // Status Handle Tests
    // =============================================================================

    #[tokio::test]
    async fn test_status_handle_returns_handle() {
        let params = ReactionBaseParams::new("r1", vec![]);
        let base = ReactionBase::new(params);

        let handle = base.status_handle();
        // The handle should share the same status as the base
        assert_eq!(handle.get_status().await, ComponentStatus::Stopped);

        // Mutating via handle should be visible via base
        handle.set_status(ComponentStatus::Running, None).await;
        assert_eq!(base.get_status().await, ComponentStatus::Running);
    }

    // =============================================================================
    // Deprovision Tests
    // =============================================================================

    #[tokio::test]
    async fn test_deprovision_common_noop_without_state_store() {
        let params = ReactionBaseParams::new("r1", vec![]);
        let base = ReactionBase::new(params);
        // Should succeed without panicking when no state store is configured
        let result = base.deprovision_common().await;
        assert!(result.is_ok());
    }

    // =============================================================================
    // Processing Task Tests
    // =============================================================================

    #[tokio::test]
    async fn test_set_processing_task_stores_handle() {
        let params = ReactionBaseParams::new("r1", vec![]);
        let base = ReactionBase::new(params);

        // Initially no processing task
        assert!(base.processing_task.read().await.is_none());

        let task = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        base.set_processing_task(task).await;

        // Now it should be stored
        assert!(base.processing_task.read().await.is_some());

        // Clean up: abort the long-running task
        let task = base.processing_task.write().await.take();
        if let Some(t) = task {
            t.abort();
        }
    }

    #[tokio::test]
    async fn test_checkpoint_read_write_round_trip() {
        let (graph, _rx) = crate::component_graph::ComponentGraph::new("test-instance");
        let update_tx = graph.update_sender();

        let params =
            ReactionBaseParams::new("ckpt-reaction", vec!["q1".to_string(), "q2".to_string()]);
        let base = ReactionBase::new(params);

        // Wire up an in-memory state store via context
        let store: Arc<dyn StateStoreProvider> =
            Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let context = crate::context::ReactionRuntimeContext::new(
            "test-instance",
            "ckpt-reaction",
            Some(store),
            update_tx,
            None,
        );
        base.initialize(context).await;

        // Initially no checkpoints
        assert!(base.read_checkpoint("q1").await.unwrap().is_none());
        assert!(base.read_all_checkpoints().await.unwrap().is_empty());

        // Write a checkpoint for q1
        let cp1 = ReactionCheckpoint {
            sequence: 10,
            config_hash: 42,
        };
        base.write_checkpoint("q1", &cp1).await.unwrap();

        // Read it back
        let read = base.read_checkpoint("q1").await.unwrap().unwrap();
        assert_eq!(read, cp1);

        // q2 still absent
        assert!(base.read_checkpoint("q2").await.unwrap().is_none());

        // Write q2 and check read_all_checkpoints
        let cp2 = ReactionCheckpoint {
            sequence: 20,
            config_hash: 99,
        };
        base.write_checkpoint("q2", &cp2).await.unwrap();

        let all = base.read_all_checkpoints().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all["q1"], cp1);
        assert_eq!(all["q2"], cp2);
    }

    #[tokio::test]
    async fn test_checkpoint_no_state_store() {
        // Without a state store, reads return None and writes fail
        let params = ReactionBaseParams::new("no-store", vec!["q1".to_string()]);
        let base = ReactionBase::new(params);

        // read_checkpoint returns None without error
        assert!(base.read_checkpoint("q1").await.unwrap().is_none());

        // read_all_checkpoints returns empty
        assert!(base.read_all_checkpoints().await.unwrap().is_empty());

        // write_checkpoint should error
        let cp = ReactionCheckpoint {
            sequence: 1,
            config_hash: 0,
        };
        assert!(base.write_checkpoint("q1", &cp).await.is_err());
    }

    #[tokio::test]
    async fn test_loop_completion_waits_for_the_final_handler_and_checkpoint() {
        let base = store_backed_base("delayed-completion").await;
        let checkpoint = ReactionCheckpoint {
            sequence: 5,
            config_hash: 42,
        };
        base.write_checkpoint("q1", &checkpoint).await.unwrap();
        let checkpoints = HashMap::from([("q1".to_string(), checkpoint)]);
        let shutdown_rx = base.create_shutdown_channel().await;
        for sequence in [6, 7] {
            base.enqueue_query_result(query_result("q1", sequence, 100))
                .await
                .unwrap();
        }
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let processed = Arc::new(Mutex::new(Vec::new()));
        let loop_base = base.clone_shared();
        let loop_entered = entered.clone();
        let loop_release = release.clone();
        let loop_processed = processed.clone();
        let task = tokio::spawn(async move {
            loop_base
                .run_standard_loop(shutdown_rx, checkpoints, |event| {
                    let entered = loop_entered.clone();
                    let release = loop_release.clone();
                    let processed = loop_processed.clone();
                    async move {
                        if event.sequence == 7 {
                            entered.notify_one();
                            release.notified().await;
                        }
                        processed.lock().await.push(event.sequence);
                        Ok(())
                    }
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(30), entered.notified())
            .await
            .unwrap();
        assert_eq!(base.priority_queue.metrics().await.total_dequeued, 2);
        let completed = finish_queued_loop(&base, 2, task);
        tokio::pin!(completed);
        assert!(
            futures::poll!(completed.as_mut()).is_pending(),
            "completion must wait for the handler, even after every input was dequeued"
        );
        assert_eq!(*processed.lock().await, vec![6]);
        assert_eq!(
            base.read_checkpoint("q1").await.unwrap().unwrap().sequence,
            6
        );
        release.notify_one();
        completed.await;
        assert_eq!(*processed.lock().await, vec![6, 7]);
        assert_eq!(
            base.read_checkpoint("q1").await.unwrap().unwrap(),
            ReactionCheckpoint {
                sequence: 7,
                config_hash: 42
            }
        );
    }

    #[tokio::test]
    async fn test_run_standard_loop_dedup_and_checkpoint() {
        assert_standard_loop_sequence_order([0, 1, 2, 2]).await;
    }

    #[tokio::test]
    async fn test_run_standard_loop_all_equal_timestamps() {
        assert_standard_loop_sequence_order([0, 0, 0, 0]).await;
    }

    #[tokio::test]
    async fn test_run_standard_loop_backwards_timestamps() {
        assert_standard_loop_sequence_order([3, 2, 1, 0]).await;
    }

    async fn assert_standard_loop_sequence_order(timestamp_offsets: [i64; 4]) {
        for iteration in 0..100 {
            let base = store_backed_base("loop-reaction").await;
            let initial_checkpoints = HashMap::from([(
                "q1".to_string(),
                ReactionCheckpoint {
                    sequence: 5,
                    config_hash: 42,
                },
            )]);
            let shutdown_rx = base.create_shutdown_channel().await;

            // Queue the whole batch before starting the real checkpointing loop.
            for (seq, offset) in [3u64, 5, 6, 7].into_iter().zip(timestamp_offsets) {
                base.enqueue_query_result(query_result("q1", seq, 1_000 + offset))
                    .await
                    .unwrap();
            }

            let processed = collect_queued_results(&base, shutdown_rx, initial_checkpoints).await;
            let cp = base.read_checkpoint("q1").await.unwrap().unwrap();
            assert_eq!(cp.sequence, 7);
            assert_eq!(cp.config_hash, 42);
            assert_eq!(
                processed
                    .iter()
                    .map(|event| event.sequence)
                    .collect::<Vec<_>>(),
                vec![6, 7],
                "lost a new result on iteration {iteration} after checkpointing seq={}",
                cp.sequence
            );
        }
    }
}
