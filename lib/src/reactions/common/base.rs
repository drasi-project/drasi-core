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

use crate::channels::priority_queue::PriorityQueue;
use crate::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, QueryResult,
};
use crate::context::ReactionRuntimeContext;
use crate::reactions::QuerySubscriber;
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
}

impl ReactionBaseParams {
    /// Create new params with ID and queries, using defaults for everything else
    pub fn new(id: impl Into<String>, queries: Vec<String>) -> Self {
        Self {
            id: id.into(),
            queries,
            priority_queue_capacity: None,
            auto_start: true, // Default to true like queries
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
}

/// Base implementation for common reaction functionality
pub struct ReactionBase {
    /// Reaction identifier
    pub id: String,
    /// List of query IDs to subscribe to
    pub queries: Vec<String>,
    /// Whether this reaction should auto-start
    pub auto_start: bool,
    /// Current component status
    pub status: Arc<RwLock<ComponentStatus>>,
    /// Runtime context (set by initialize())
    context: Arc<RwLock<Option<ReactionRuntimeContext>>>,
    /// Channel for sending component status events (extracted from context for convenience)
    status_tx: Arc<RwLock<Option<ComponentEventSender>>>,
    /// Query subscriber for accessing queries (extracted from context)
    query_subscriber: Arc<RwLock<Option<Arc<dyn QuerySubscriber>>>>,
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
}

impl ReactionBase {
    /// Create a new ReactionBase with the given parameters
    ///
    /// Dependencies (event channel, query subscriber, state store) are not required during
    /// construction - they will be provided via `initialize()` when the reaction is added to DrasiLib.
    pub fn new(params: ReactionBaseParams) -> Self {
        Self {
            priority_queue: PriorityQueue::new(params.priority_queue_capacity.unwrap_or(10000)),
            id: params.id,
            queries: params.queries,
            auto_start: params.auto_start,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            context: Arc::new(RwLock::new(None)), // Set by initialize()
            status_tx: Arc::new(RwLock::new(None)), // Extracted from context
            query_subscriber: Arc::new(RwLock::new(None)), // Extracted from context
            state_store: Arc::new(RwLock::new(None)), // Extracted from context
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            processing_task: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
        }
    }

    /// Initialize the reaction with runtime context.
    ///
    /// This method is called automatically by DrasiLib's `add_reaction()` method.
    /// Plugin developers do not need to call this directly.
    ///
    /// The context provides access to:
    /// - `reaction_id`: The reaction's unique identifier
    /// - `status_tx`: Channel for reporting component status events
    /// - `state_store`: Optional persistent state storage
    /// - `query_subscriber`: Access to query instances for subscription
    pub async fn initialize(&self, context: ReactionRuntimeContext) {
        // Store context for later use
        *self.context.write().await = Some(context.clone());

        // Extract services for convenience
        *self.status_tx.write().await = Some(context.status_tx.clone());
        *self.query_subscriber.write().await = Some(context.query_subscriber.clone());

        if let Some(state_store) = context.state_store.as_ref() {
            *self.state_store.write().await = Some(state_store.clone());
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

    /// Get whether this reaction should auto-start
    pub fn get_auto_start(&self) -> bool {
        self.auto_start
    }

    /// Get the status channel Arc for internal use by spawned tasks
    ///
    /// This returns the internal status_tx wrapped in Arc<RwLock<Option<...>>>
    /// which allows background tasks to send component status events.
    ///
    /// Returns a clone of the Arc that can be moved into spawned tasks.
    pub fn status_tx(&self) -> Arc<RwLock<Option<ComponentEventSender>>> {
        self.status_tx.clone()
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
            status: self.status.clone(),
            context: self.context.clone(),
            status_tx: self.status_tx.clone(),
            query_subscriber: self.query_subscriber.clone(),
            state_store: self.state_store.clone(),
            priority_queue: self.priority_queue.clone(),
            subscription_tasks: self.subscription_tasks.clone(),
            processing_task: self.processing_task.clone(),
            shutdown_tx: self.shutdown_tx.clone(),
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

    /// Get current status
    pub async fn get_status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    /// Send a component lifecycle event
    ///
    /// If the event channel has not been injected yet, this method silently
    /// succeeds without sending anything. This allows reactions to be used
    /// in a standalone fashion without DrasiLib if needed.
    pub async fn send_component_event(
        &self,
        status: ComponentStatus,
        message: Option<String>,
    ) -> Result<()> {
        let event = ComponentEvent {
            component_id: self.id.clone(),
            component_type: ComponentType::Reaction,
            status,
            timestamp: chrono::Utc::now(),
            message,
        };

        if let Some(ref tx) = *self.status_tx.read().await {
            if let Err(e) = tx.send(event).await {
                error!("Failed to send component event: {e}");
            }
        }
        // If status_tx is None, silently skip - initialization happens before start()
        Ok(())
    }

    /// Transition to a new status and send event
    pub async fn set_status_with_event(
        &self,
        status: ComponentStatus,
        message: Option<String>,
    ) -> Result<()> {
        *self.status.write().await = status.clone();
        self.send_component_event(status, message).await
    }

    /// Subscribe to all configured queries and spawn forwarder tasks
    ///
    /// This method handles the common pattern of:
    /// 1. Getting query instances via the injected QuerySubscriber
    /// 2. Subscribing to each configured query
    /// 3. Spawning forwarder tasks to enqueue results to priority queue
    ///
    /// # Prerequisites
    /// * `inject_query_subscriber()` must have been called (done automatically by DrasiLib)
    ///
    /// # Returns
    /// * `Ok(())` if all subscriptions succeeded
    /// * `Err(...)` if QuerySubscriber not injected or any subscription failed
    pub async fn subscribe_to_queries(&self) -> Result<()> {
        // Get the injected query subscriber (clone the Arc to release the lock)
        let query_subscriber = {
            let qs_guard = self.query_subscriber.read().await;
            qs_guard.as_ref().cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "QuerySubscriber not injected - was reaction '{}' added to DrasiLib?",
                    self.id
                )
            })?
        };

        // Subscribe to all configured queries and spawn forwarder tasks
        for query_id in &self.queries {
            // Get the query instance via QuerySubscriber
            let query = query_subscriber.get_query_instance(query_id).await?;

            // Subscribe to the query
            let subscription_response = query
                .subscribe(self.id.clone())
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            let mut receiver = subscription_response.receiver;

            // Clone necessary data for the forwarder task
            let priority_queue = self.priority_queue.clone();
            let query_id_clone = query_id.clone();
            let reaction_id = self.id.clone();

            // Get query dispatch mode to determine enqueue strategy
            let query_config = query.get_config();
            let dispatch_mode = query_config
                .dispatch_mode
                .unwrap_or(crate::channels::DispatchMode::Channel);
            let use_blocking_enqueue =
                matches!(dispatch_mode, crate::channels::DispatchMode::Channel);

            // Spawn forwarder task to read from receiver and enqueue to priority queue
            let forwarder_task = tokio::spawn(async move {
                debug!(
                    "[{reaction_id}] Started result forwarder for query '{query_id_clone}' (dispatch_mode: {dispatch_mode:?}, blocking_enqueue: {use_blocking_enqueue})"
                );

                loop {
                    match receiver.recv().await {
                        Ok(query_result) => {
                            // Use appropriate enqueue method based on dispatch mode
                            if use_blocking_enqueue {
                                // Channel mode: Use blocking enqueue to prevent message loss
                                // This creates backpressure when the priority queue is full
                                priority_queue.enqueue_wait(query_result).await;
                            } else {
                                // Broadcast mode: Use non-blocking enqueue to prevent deadlock
                                // Messages may be dropped when priority queue is full
                                if !priority_queue.enqueue(query_result).await {
                                    warn!(
                                        "[{reaction_id}] Failed to enqueue result from query '{query_id_clone}' - priority queue at capacity (broadcast mode)"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            // Check if it's a lag error or closed channel
                            let error_str = e.to_string();
                            if error_str.contains("lagged") {
                                warn!(
                                    "[{reaction_id}] Receiver lagged for query '{query_id_clone}': {error_str}"
                                );
                                continue;
                            } else {
                                info!(
                                    "[{reaction_id}] Receiver error for query '{query_id_clone}': {error_str}"
                                );
                                break;
                            }
                        }
                    }
                }
            });

            // Store the forwarder task handle
            self.subscription_tasks.write().await.push(forwarder_task);
        }

        Ok(())
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
        if let Some(task) = processing_task.take() {
            // Give the task a short time to respond to the shutdown signal
            match tokio::time::timeout(std::time::Duration::from_secs(2), task).await {
                Ok(Ok(())) => {
                    debug!("[{}] Processing task completed gracefully", self.id);
                }
                Ok(Err(e)) => {
                    // Task was aborted or panicked
                    debug!("[{}] Processing task ended: {}", self.id, e);
                }
                Err(_) => {
                    // Timeout - task didn't respond to shutdown signal
                    // This shouldn't happen if the task is using tokio::select! correctly
                    warn!(
                        "[{}] Processing task did not respond to shutdown signal within timeout",
                        self.id
                    );
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

        Ok(())
    }

    /// Set the processing task handle
    pub async fn set_processing_task(&self, task: tokio::task::JoinHandle<()>) {
        *self.processing_task.write().await = Some(task);
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
        use crate::queries::Query;

        // Mock QuerySubscriber for testing
        struct MockQuerySubscriber;

        #[async_trait::async_trait]
        impl crate::reactions::QuerySubscriber for MockQuerySubscriber {
            async fn get_query_instance(
                &self,
                _id: &str,
            ) -> anyhow::Result<std::sync::Arc<dyn Query>> {
                Err(anyhow::anyhow!("MockQuerySubscriber: query not found"))
            }
        }

        let (status_tx, mut event_rx) = mpsc::channel(100);
        let params = ReactionBaseParams::new("test-reaction", vec![]);

        let base = ReactionBase::new(params);

        // Create context and initialize
        let context = ReactionRuntimeContext::new(
            "test-reaction",
            status_tx,
            None,
            std::sync::Arc::new(MockQuerySubscriber),
        );
        base.initialize(context).await;

        // Test status transition
        base.set_status_with_event(ComponentStatus::Starting, Some("Starting test".to_string()))
            .await
            .unwrap();

        assert_eq!(base.get_status().await, ComponentStatus::Starting);

        // Check event was sent
        let event = event_rx.try_recv().unwrap();
        assert_eq!(event.status, ComponentStatus::Starting);
        assert_eq!(event.message, Some("Starting test".to_string()));
    }

    #[tokio::test]
    async fn test_priority_queue_operations() {
        let params =
            ReactionBaseParams::new("test-reaction", vec![]).with_priority_queue_capacity(10);

        let base = ReactionBase::new(params);

        // Create a test query result
        let query_result = QueryResult::new(
            "test-query".to_string(),
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
        // Test that send_component_event works even without context initialization
        let params = ReactionBaseParams::new("test-reaction", vec![]);

        let base = ReactionBase::new(params);

        // This should succeed without panicking (silently does nothing when status_tx is None)
        base.send_component_event(ComponentStatus::Starting, None)
            .await
            .unwrap();
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

        // Call stop_common - should send shutdown signal
        let _ = base.stop_common().await;

        // Give task time to process
        tokio::time::sleep(Duration::from_millis(50)).await;

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
}
