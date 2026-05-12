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
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::Instrument;

use crate::channels::priority_queue::PriorityQueue;
use crate::channels::{ComponentStatus, QueryResult};
use crate::component_graph::ComponentStatusHandle;
use crate::context::ReactionRuntimeContext;
use crate::identity::IdentityProvider;
use crate::reactions::checkpoint::ReactionCheckpoint;
use crate::recovery::ReactionRecoveryPolicy;
use crate::state_store::StateStoreProvider;

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
    /// Priority queue for timestamp-ordered result processing
    pub priority_queue: PriorityQueue<QueryResult>,
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
    /// This should be called before spawning the processing task.
    pub async fn create_shutdown_channel(&self) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *self.shutdown_tx.write().await = Some(tx);
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

    /// Returns a clonable [`ComponentStatusHandle`] for use in spawned tasks.
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
    /// Results are processed in timestamp order by the reaction's processing task.
    pub async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.priority_queue.enqueue_wait(Arc::new(result)).await;
        Ok(())
    }

    // ========================================================================
    // Checkpoint helpers
    // ========================================================================

    /// Read the persisted checkpoint for a single query subscription.
    ///
    /// Returns `Ok(None)` if no checkpoint exists (fresh start).
    /// Errors propagate from the state store or deserialization.
    pub async fn read_checkpoint(
        &self,
        query_id: &str,
    ) -> Result<Option<ReactionCheckpoint>> {
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
        let store = store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No state store configured — cannot write checkpoint"))?;
        crate::reactions::checkpoint::write_checkpoint(store.as_ref(), &self.id, query_id, checkpoint)
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
                }
            }
        }
        drop(processing_task);

        // Drain the priority queue
        let drained_events = self.priority_queue.drain().await;
        if !drained_events.is_empty() {
            info!(
                "[{}] Drained {} pending events from priority queue",
                self.id,
                drained_events.len()
            );
        }

        self.set_status(
            ComponentStatus::Stopped,
            Some(format!("Reaction '{}' stopped", self.id)),
        )
        .await;
        info!("Reaction '{}' stopped", self.id);

        Ok(())
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

        // Create second channel (should replace the first)
        let mut rx2 = base.create_shutdown_channel().await;

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
    async fn test_run_standard_loop_dedup_and_checkpoint() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let (graph, _rx) = crate::component_graph::ComponentGraph::new("test-instance");
        let update_tx = graph.update_sender();

        let params = ReactionBaseParams::new("loop-reaction", vec!["q1".to_string()]);
        let base = ReactionBase::new(params);

        let store: Arc<dyn StateStoreProvider> =
            Arc::new(crate::state_store::MemoryStateStoreProvider::new());
        let context = crate::context::ReactionRuntimeContext::new(
            "test-instance",
            "loop-reaction",
            Some(store),
            update_tx,
            None,
        );
        base.initialize(context).await;

        // Initial checkpoints: seq=5 with config_hash=42
        let initial_checkpoints = {
            let mut m = std::collections::HashMap::new();
            m.insert(
                "q1".to_string(),
                ReactionCheckpoint {
                    sequence: 5,
                    config_hash: 42,
                },
            );
            m
        };

        // Enqueue events: seq 3 (dup), seq 5 (dup), seq 6, seq 7
        for seq in [3u64, 5, 6, 7] {
            let result = crate::channels::QueryResult {
                query_id: "q1".to_string(),
                sequence: seq,
                timestamp: chrono::Utc::now(),
                results: vec![],
                metadata: Default::default(),
                profiling: None,
            };
            base.enqueue_query_result(result).await.unwrap();
        }

        // Track which sequences the handler actually processes
        let processed = Arc::new(tokio::sync::Mutex::new(Vec::<u64>::new()));
        let processed_clone = processed.clone();
        let handler_count = Arc::new(AtomicU64::new(0));
        let handler_count_clone = handler_count.clone();

        let shutdown_rx = base.create_shutdown_channel().await;
        let base_clone = base.clone_shared();

        let loop_handle = tokio::spawn(async move {
            base_clone
                .run_standard_loop(shutdown_rx, initial_checkpoints, |event| {
                    let processed = processed_clone.clone();
                    let count = handler_count_clone.clone();
                    async move {
                        processed.lock().await.push(event.sequence);
                        count.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await
                .unwrap();
        });

        // Wait for all non-dup events to be processed (seq 6 and 7)
        for _ in 0..50 {
            if handler_count.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Signal shutdown via stop_common (the standard path)
        let _ = base.stop_common().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), loop_handle).await;

        // Verify only seq 6 and 7 were processed (3 and 5 were deduped)
        let processed = processed.lock().await;
        assert_eq!(*processed, vec![6, 7]);

        // Checkpoint should now be at seq=7, preserving config_hash=42
        let cp = base.read_checkpoint("q1").await.unwrap().unwrap();
        assert_eq!(cp.sequence, 7);
        assert_eq!(cp.config_hash, 42);
    }
}
