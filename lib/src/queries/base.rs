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

//! Base implementation for common query functionality.
//!
//! This module provides `QueryBase` which encapsulates common patterns
//! used across all query implementations:
//! - Dispatcher setup and management
//! - Result dispatching with profiling
//! - Component lifecycle management
//!
//! ## Usage
//!
//! Query implementations should create a `QueryBase` instance and use
//! its methods to handle common operations:
//!
//! ```ignore
//! use drasi_lib::queries::base::QueryBase;
//! use drasi_lib::config::QueryConfig;
//! use drasi_lib::channels::ComponentEventSender;
//! use anyhow::Result;
//!
//! pub struct MyQuery {
//!     base: QueryBase,
//!     // ... query-specific fields
//! }
//!
//! impl MyQuery {
//!     pub fn new(config: QueryConfig, event_tx: ComponentEventSender) -> Result<Self> {
//!         let base = QueryBase::new(config, event_tx)?;
//!         Ok(Self { base /* ... */ })
//!     }
//! }
//! ```

use anyhow::Result;
use log::{debug, error, info};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::{
    BroadcastChangeDispatcher, ChangeDispatcher, ChangeReceiver, ChannelChangeDispatcher,
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, DispatchMode,
    QueryResult, QuerySubscriptionResponse,
};
use crate::config::QueryConfig;
// Profiling will be used when implementing performance tracking
use chrono::Utc;

/// Base implementation for common query functionality
pub struct QueryBase {
    /// Query configuration
    pub config: QueryConfig,
    /// Current component status
    pub status: Arc<RwLock<ComponentStatus>>,
    /// Dispatchers for sending query results to subscribers
    pub dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>>,
    /// Channel for sending component lifecycle events
    pub event_tx: ComponentEventSender,
    /// Handle to the query's main task
    pub task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Sender for shutdown signal
    pub shutdown_tx: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl QueryBase {
    /// Create a new QueryBase with the given configuration
    pub fn new(config: QueryConfig, event_tx: ComponentEventSender) -> Result<Self> {
        // Determine dispatch mode (default to Channel if not specified)
        let dispatch_mode = config.dispatch_mode.unwrap_or_default();

        // Set up initial dispatchers based on dispatch mode
        let mut dispatchers: Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>> = Vec::new();

        if dispatch_mode == DispatchMode::Broadcast {
            // For broadcast mode, create a single broadcast dispatcher
            let capacity = config.dispatch_buffer_capacity.unwrap_or(1000);
            let dispatcher = BroadcastChangeDispatcher::<QueryResult>::new(capacity);
            dispatchers.push(Box::new(dispatcher));
        }
        // For channel mode, dispatchers will be created on-demand when subscribing

        Ok(Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            dispatchers: Arc::new(RwLock::new(dispatchers)),
            event_tx,
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
        })
    }

    /// Subscribe to this query's results
    ///
    /// For broadcast mode: returns a receiver from the single broadcast dispatcher
    /// For channel mode: creates a new channel dispatcher and returns its receiver
    pub async fn subscribe(&self, reaction_id: &str) -> Result<QuerySubscriptionResponse> {
        info!(
            "Query '{}' received subscription from reaction '{}'",
            self.config.id, reaction_id
        );

        let dispatch_mode = self.config.dispatch_mode.unwrap_or_default();

        let receiver: Box<dyn ChangeReceiver<QueryResult>> = match dispatch_mode {
            DispatchMode::Broadcast => {
                // For broadcast mode, use the single dispatcher
                let dispatchers = self.dispatchers.read().await;
                if let Some(dispatcher) = dispatchers.first() {
                    dispatcher.create_receiver().await?
                } else {
                    return Err(anyhow::anyhow!("No broadcast dispatcher available"));
                }
            }
            DispatchMode::Channel => {
                // For channel mode, create a new dispatcher for this subscription
                let capacity = self.config.dispatch_buffer_capacity.unwrap_or(1000);
                let dispatcher = ChannelChangeDispatcher::<QueryResult>::new(capacity);
                let receiver = dispatcher.create_receiver().await?;

                // Add the new dispatcher to our list
                let mut dispatchers = self.dispatchers.write().await;
                dispatchers.push(Box::new(dispatcher));

                receiver
            }
        };

        Ok(QuerySubscriptionResponse {
            query_id: self.config.id.clone(),
            receiver,
        })
    }

    /// Dispatch query results to all subscribers
    ///
    /// This method handles the common pattern of:
    /// - Creating profiling metadata with timestamp
    /// - Arc-wrapping for efficient sharing
    /// - Dispatching to all dispatchers
    /// - Handling the no-subscriber case gracefully
    pub async fn dispatch_query_result(&self, result: QueryResult) -> Result<()> {
        debug!(
            "[{}] Dispatching query result with {} results",
            self.config.id,
            result.results.len()
        );

        // Arc-wrap for zero-copy sharing across dispatchers
        let arc_result = Arc::new(result);

        // Send to all dispatchers
        let dispatchers = self.dispatchers.read().await;
        for dispatcher in dispatchers.iter() {
            if let Err(e) = dispatcher.dispatch_change(arc_result.clone()).await {
                debug!("[{}] Failed to dispatch result: {}", self.config.id, e);
            }
        }

        Ok(())
    }

    /// Dispatch multiple query results to all subscribers
    pub async fn dispatch_query_results(&self, results: Vec<QueryResult>) -> Result<()> {
        for result in results {
            self.dispatch_query_result(result).await?;
        }
        Ok(())
    }

    /// Update status and emit a lifecycle event
    pub async fn emit_status_event(&self, status: ComponentStatus, message: Option<&str>) {
        *self.status.write().await = status.clone();

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Query,
            status,
            timestamp: Utc::now(),
            message: message.map(|m| m.to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }
    }

    /// Handle common stop functionality
    pub async fn stop_common(&self) -> Result<()> {
        info!("Stopping query '{}'", self.config.id);

        // Send shutdown signal if we have one
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }

        // Wait for task to complete
        if let Some(handle) = self.task_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    info!("Query '{}' task completed successfully", self.config.id);
                }
                Ok(Err(e)) => {
                    error!("Query '{}' task panicked: {}", self.config.id, e);
                }
                Err(_) => {
                    error!(
                        "Query '{}' task did not complete within timeout",
                        self.config.id
                    );
                }
            }
        }

        *self.status.write().await = ComponentStatus::Stopped;
        info!("Query '{}' stopped", self.config.id);
        Ok(())
    }

    /// Get the current status
    pub async fn get_status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    /// Set the task handle
    pub async fn set_task_handle(&self, handle: tokio::task::JoinHandle<()>) {
        *self.task_handle.write().await = Some(handle);
    }

    /// Set the shutdown sender
    pub async fn set_shutdown_tx(&self, tx: tokio::sync::oneshot::Sender<()>) {
        *self.shutdown_tx.write().await = Some(tx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QueryLanguage;
    use std::collections::HashMap;

    #[tokio::test]
    async fn test_query_base_broadcast_mode() {
        let config = QueryConfig {
            id: "test_query".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![],
            auto_start: false,
            joins: None,
            enable_bootstrap: false,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: Some(100),
            dispatch_mode: Some(DispatchMode::Broadcast),
            storage_backend: None,
        };

        let (event_tx, _) = tokio::sync::mpsc::channel(100);
        let base = QueryBase::new(config, event_tx).unwrap();

        // Subscribe multiple times
        let sub1 = base.subscribe("reaction1").await.unwrap();
        let sub2 = base.subscribe("reaction2").await.unwrap();

        // Dispatch a result
        let result = QueryResult {
            query_id: "test_query".to_string(),
            timestamp: chrono::Utc::now(),
            results: vec![],
            metadata: HashMap::new(),
            profiling: None,
        };

        base.dispatch_query_result(result.clone()).await.unwrap();

        // Both subscribers should receive the result
        let mut receiver1 = sub1.receiver;
        let mut receiver2 = sub2.receiver;

        let received1 = receiver1.recv().await.unwrap();
        let received2 = receiver2.recv().await.unwrap();

        assert_eq!(received1.query_id, "test_query");
        assert_eq!(received2.query_id, "test_query");
    }

    #[tokio::test]
    async fn test_query_base_channel_mode() {
        let config = QueryConfig {
            id: "test_query".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            middleware: vec![],
            sources: vec![],
            auto_start: false,
            joins: None,
            enable_bootstrap: false,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: Some(100),
            dispatch_mode: Some(DispatchMode::Channel),
            storage_backend: None,
        };

        let (event_tx, _) = tokio::sync::mpsc::channel(100);
        let base = QueryBase::new(config, event_tx).unwrap();

        // Subscribe multiple times - each gets its own channel
        let sub1 = base.subscribe("reaction1").await.unwrap();
        let sub2 = base.subscribe("reaction2").await.unwrap();

        // Dispatch a result
        let result = QueryResult {
            query_id: "test_query".to_string(),
            timestamp: chrono::Utc::now(),
            results: vec![],
            metadata: HashMap::new(),
            profiling: None,
        };

        base.dispatch_query_result(result.clone()).await.unwrap();

        // Both subscribers should receive the result via their own channels
        let mut receiver1 = sub1.receiver;
        let mut receiver2 = sub2.receiver;

        let received1 = receiver1.recv().await.unwrap();
        let received2 = receiver2.recv().await.unwrap();

        assert_eq!(received1.query_id, "test_query");
        assert_eq!(received2.query_id, "test_query");
    }
}
