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

//! Plugin core module for reaction abstractions
//!
//! This module provides the core traits and base implementations that all reaction
//! plugins must implement. It separates the plugin contract from the reaction manager.

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, warn};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::priority_queue::PriorityQueue;
use crate::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, QueryResult,
};
use crate::config::ReactionConfig;
use crate::queries::Query;

/// Trait for subscribing to queries without requiring full DrasiLib dependency
#[async_trait]
pub trait QuerySubscriber: Send + Sync {
    /// Get a query instance by ID
    async fn get_query_instance(&self, id: &str) -> Result<Arc<dyn Query>>;
}

/// Trait defining the interface for all reaction implementations
///
/// # Subscription Model
///
/// Reactions manage their own subscriptions to queries using the broadcast channel pattern:
/// - Each reaction receives a reference to a QuerySubscriber on startup
/// - Reactions access queries via `query_subscriber.get_query_instance()`
/// - For each query, reactions call `query.subscribe(reaction_id)`
/// - Each subscription provides a broadcast receiver for that query's results
/// - Reactions use a priority queue to process results from multiple queries in timestamp order
#[async_trait]
pub trait Reaction: Send + Sync {
    /// Start the reaction with access to query subscriptions
    async fn start(&self, query_subscriber: Arc<dyn QuerySubscriber>) -> Result<()>;

    /// Stop the reaction, cleaning up all subscriptions and tasks
    async fn stop(&self) -> Result<()>;

    /// Get the current status of the reaction
    async fn status(&self) -> ComponentStatus;

    /// Get the reaction's configuration
    fn get_config(&self) -> &ReactionConfig;
}

/// Base implementation for common reaction functionality
///
/// This encapsulates common patterns used across all reaction implementations:
/// - Query subscription management
/// - Priority queue handling
/// - Task lifecycle management
/// - Component status tracking
/// - Event reporting
pub struct ReactionBase {
    /// Reaction configuration
    pub config: ReactionConfig,
    /// Current component status
    pub status: Arc<RwLock<ComponentStatus>>,
    /// Channel for sending component lifecycle events
    pub event_tx: ComponentEventSender,
    /// Priority queue for timestamp-ordered result processing
    pub priority_queue: PriorityQueue<QueryResult>,
    /// Handles to subscription forwarder tasks
    pub subscription_tasks: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
    /// Handle to the main processing task
    pub processing_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl ReactionBase {
    /// Create a new ReactionBase with the given configuration
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        Self {
            priority_queue: PriorityQueue::new(config.priority_queue_capacity.unwrap_or(10000)),
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            processing_task: Arc::new(RwLock::new(None)),
        }
    }

    /// Get current status
    pub async fn get_status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    /// Send a component lifecycle event
    pub async fn send_component_event(
        &self,
        status: ComponentStatus,
        message: Option<String>,
    ) -> Result<()> {
        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status,
            timestamp: chrono::Utc::now(),
            message,
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

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
    pub async fn subscribe_to_queries(
        &self,
        query_subscriber: Arc<dyn QuerySubscriber>,
    ) -> Result<()> {
        // Subscribe to all configured queries and spawn forwarder tasks
        for query_id in &self.config.queries {
            // Get the query instance via QuerySubscriber
            let query = query_subscriber.get_query_instance(query_id).await?;

            // Subscribe to the query
            let subscription_response = query
                .subscribe(self.config.id.clone())
                .await
                .map_err(|e| anyhow::anyhow!(e))?;
            let mut receiver = subscription_response.receiver;

            // Clone necessary data for the forwarder task
            let priority_queue = self.priority_queue.clone();
            let query_id_clone = query_id.clone();
            let reaction_id = self.config.id.clone();

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
    pub async fn stop_common(&self) -> Result<()> {
        info!("Stopping reaction: {}", self.config.id);

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
                self.config.id,
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

/// Factory for creating reaction instances from configuration
pub struct ReactionFactory;

impl ReactionFactory {
    /// Create a reaction instance from configuration
    ///
    /// This will be expanded to support plugins in the future.
    /// For now, it serves as a placeholder for the plugin architecture.
    pub fn create(_config: &ReactionConfig, _event_tx: ComponentEventSender) -> Result<Arc<dyn Reaction>> {
        // Placeholder - actual implementations will be registered via plugins
        Err(anyhow::anyhow!("Reaction creation via factory not yet implemented. Use ReactionManager for now."))
    }
}
