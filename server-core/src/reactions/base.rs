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

use anyhow::Result;
use log::{error, info, warn};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::priority_queue::PriorityQueue;
use crate::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, QueryResult,
};
use crate::config::ReactionConfig;
use crate::server_core::DrasiServerCore;

/// Base implementation for common reaction functionality
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
    ///
    /// This method handles the common pattern of:
    /// 1. Getting QueryManager from server_core
    /// 2. Subscribing to each configured query
    /// 3. Spawning forwarder tasks to enqueue results to priority queue
    ///
    /// # Arguments
    /// * `server_core` - Reference to DrasiServerCore for accessing QueryManager
    ///
    /// # Returns
    /// * `Ok(())` if all subscriptions succeeded
    /// * `Err(...)` if any subscription failed
    pub async fn subscribe_to_queries(&self, server_core: Arc<DrasiServerCore>) -> Result<()> {
        // Get QueryManager from server_core
        let query_manager = server_core.query_manager();

        // Subscribe to all configured queries and spawn forwarder tasks
        for query_id in &self.config.queries {
            // Get the query instance
            let query = query_manager
                .get_query_instance(query_id)
                .await
                .map_err(|e| anyhow::anyhow!(e))?;

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

            // Spawn forwarder task to read from receiver and enqueue to priority queue
            let forwarder_task = tokio::spawn(async move {
                loop {
                    match receiver.recv().await {
                        Ok(query_result) => {
                            // Enqueue to priority queue for timestamp-ordered processing
                            if !priority_queue.enqueue(query_result).await {
                                warn!(
                                    "[{}] Failed to enqueue result from query '{}' - priority queue at capacity",
                                    reaction_id, query_id_clone
                                );
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_reaction_base_creation() {
        let (event_tx, _event_rx) = mpsc::channel(100);
        let config = ReactionConfig {
            id: "test-reaction".to_string(),
            reaction_type: "test".to_string(),
            queries: vec!["query1".to_string()],
            auto_start: true,
            properties: Default::default(),
            priority_queue_capacity: Some(5000),
        };

        let base = ReactionBase::new(config.clone(), event_tx);
        assert_eq!(base.config.id, "test-reaction");
        assert_eq!(base.get_status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_status_transitions() {
        let (event_tx, mut event_rx) = mpsc::channel(100);
        let config = ReactionConfig {
            id: "test-reaction".to_string(),
            reaction_type: "test".to_string(),
            queries: vec![],
            auto_start: true,
            properties: Default::default(),
            priority_queue_capacity: None,
        };

        let base = ReactionBase::new(config, event_tx);

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
        let (event_tx, _event_rx) = mpsc::channel(100);
        let config = ReactionConfig {
            id: "test-reaction".to_string(),
            reaction_type: "test".to_string(),
            queries: vec![],
            auto_start: true,
            properties: Default::default(),
            priority_queue_capacity: Some(10),
        };

        let base = ReactionBase::new(config, event_tx);

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
}
