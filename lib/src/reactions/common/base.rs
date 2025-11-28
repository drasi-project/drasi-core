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
use crate::plugin_core::QuerySubscriber;

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
/// let params = ReactionBaseParams {
///     id: "my-reaction".to_string(),
///     queries: vec!["query1".to_string(), "query2".to_string()],
///     priority_queue_capacity: Some(5000),
/// };
///
/// let base = ReactionBase::new(params, event_tx);
/// ```
#[derive(Debug, Clone)]
pub struct ReactionBaseParams {
    /// Unique identifier for the reaction
    pub id: String,
    /// List of query IDs this reaction subscribes to
    pub queries: Vec<String>,
    /// Priority queue capacity - defaults to 10000
    pub priority_queue_capacity: Option<usize>,
}

impl ReactionBaseParams {
    /// Create new params with ID and queries, using defaults for everything else
    pub fn new(id: impl Into<String>, queries: Vec<String>) -> Self {
        Self {
            id: id.into(),
            queries,
            priority_queue_capacity: None,
        }
    }

    /// Set the priority queue capacity
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }
}

/// Base implementation for common reaction functionality
pub struct ReactionBase {
    /// Reaction identifier
    pub id: String,
    /// List of query IDs to subscribe to
    pub queries: Vec<String>,
    /// Current component status
    pub status: Arc<RwLock<ComponentStatus>>,
    /// Channel for sending component lifecycle events (injected by DrasiLib when added)
    event_tx: Arc<RwLock<Option<ComponentEventSender>>>,
    /// Priority queue for timestamp-ordered result processing
    pub priority_queue: PriorityQueue<QueryResult>,
    /// Handles to subscription forwarder tasks
    pub subscription_tasks: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
    /// Handle to the main processing task
    pub processing_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl ReactionBase {
    /// Create a new ReactionBase with the given parameters
    ///
    /// The event channel is not required during construction - it will be
    /// injected by DrasiLib when the reaction is added via `inject_event_tx()`.
    pub fn new(params: ReactionBaseParams) -> Self {
        Self {
            priority_queue: PriorityQueue::new(params.priority_queue_capacity.unwrap_or(10000)),
            id: params.id,
            queries: params.queries,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx: Arc::new(RwLock::new(None)), // Injected later by DrasiLib
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            processing_task: Arc::new(RwLock::new(None)),
        }
    }

    /// Inject the event channel (called by DrasiLib when reaction is added)
    ///
    /// This method is called automatically by DrasiLib's `add_reaction()` method.
    /// Plugin developers do not need to call this directly.
    pub async fn inject_event_tx(&self, tx: ComponentEventSender) {
        *self.event_tx.write().await = Some(tx);
    }

    /// Get the event channel Arc for internal use by spawned tasks
    ///
    /// This returns the internal event_tx wrapped in Arc<RwLock<Option<...>>>
    /// which allows background tasks to send component events.
    ///
    /// Returns a clone of the Arc that can be moved into spawned tasks.
    pub fn event_tx(&self) -> Arc<RwLock<Option<ComponentEventSender>>> {
        self.event_tx.clone()
    }

    /// Clone the ReactionBase with shared Arc references
    ///
    /// This creates a new ReactionBase that shares the same underlying
    /// data through Arc references. Useful for passing to spawned tasks.
    pub fn clone_shared(&self) -> Self {
        Self {
            id: self.id.clone(),
            queries: self.queries.clone(),
            status: self.status.clone(),
            event_tx: self.event_tx.clone(),
            priority_queue: self.priority_queue.clone(),
            subscription_tasks: self.subscription_tasks.clone(),
            processing_task: self.processing_task.clone(),
        }
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

        if let Some(ref tx) = *self.event_tx.read().await {
            if let Err(e) = tx.send(event).await {
                error!("Failed to send component event: {}", e);
            }
        }
        // If event_tx is None, silently skip - injection happens before start()
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
    /// 1. Getting query instances via QuerySubscriber
    /// 2. Subscribing to each configured query
    /// 3. Spawning forwarder tasks to enqueue results to priority queue
    ///
    /// # Arguments
    /// * `query_subscriber` - Trait object providing access to query instances
    ///
    /// # Returns
    /// * `Ok(())` if all subscriptions succeeded
    /// * `Err(...)` if any subscription failed
    pub async fn subscribe_to_queries(
        &self,
        query_subscriber: Arc<dyn QuerySubscriber>,
    ) -> Result<()> {
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
            let use_blocking_enqueue = matches!(dispatch_mode, crate::channels::DispatchMode::Channel);

            // Spawn forwarder task to read from receiver and enqueue to priority queue
            let forwarder_task = tokio::spawn(async move {
                debug!(
                    "[{}] Started result forwarder for query '{}' (dispatch_mode: {:?}, blocking_enqueue: {})",
                    reaction_id, query_id_clone, dispatch_mode, use_blocking_enqueue
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
                                        "[{}] Failed to enqueue result from query '{}' - priority queue at capacity (broadcast mode)",
                                        reaction_id, query_id_clone
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            // Check if it's a lag error or closed channel
                            let error_str = e.to_string();
                            if error_str.contains("lagged") {
                                warn!(
                                    "[{}] Receiver lagged for query '{}': {}",
                                    reaction_id, query_id_clone, error_str
                                );
                                continue;
                            } else {
                                info!(
                                    "[{}] Receiver error for query '{}': {}",
                                    reaction_id, query_id_clone, error_str
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
    /// 1. Aborting all subscription forwarder tasks
    /// 2. Aborting the processing task
    /// 3. Draining the priority queue
    pub async fn stop_common(&self) -> Result<()> {
        info!("Stopping reaction: {}", self.id);

        // Abort all subscription forwarder tasks
        let mut subscription_tasks = self.subscription_tasks.write().await;
        for task in subscription_tasks.drain(..) {
            task.abort();
        }
        drop(subscription_tasks);

        // Abort the processing task
        let mut processing_task = self.processing_task.write().await;
        if let Some(task) = processing_task.take() {
            task.abort();
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
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_reaction_base_creation() {
        let params = ReactionBaseParams {
            id: "test-reaction".to_string(),
            queries: vec!["query1".to_string()],
            priority_queue_capacity: Some(5000),
        };

        let base = ReactionBase::new(params);
        assert_eq!(base.id, "test-reaction");
        assert_eq!(base.get_status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_status_transitions() {
        let (event_tx, mut event_rx) = mpsc::channel(100);
        let params = ReactionBaseParams {
            id: "test-reaction".to_string(),
            queries: vec![],
            priority_queue_capacity: None,
        };

        let base = ReactionBase::new(params);
        // Inject event_tx to test event sending
        base.inject_event_tx(event_tx).await;

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
        let params = ReactionBaseParams {
            id: "test-reaction".to_string(),
            queries: vec![],
            priority_queue_capacity: Some(10),
        };

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
    async fn test_event_without_injection() {
        // Test that send_component_event works even without event_tx injection
        let params = ReactionBaseParams {
            id: "test-reaction".to_string(),
            queries: vec![],
            priority_queue_capacity: None,
        };

        let base = ReactionBase::new(params);

        // This should succeed without panicking (silently does nothing)
        base.send_component_event(ComponentStatus::Starting, None)
            .await
            .unwrap();
    }
}
