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
//! use anyhow::Result;
//!
//! pub struct MyQuery {
//!     base: QueryBase,
//!     // ... query-specific fields
//! }
//!
//! impl MyQuery {
//!     pub fn new(config: QueryConfig) -> Result<Self> {
//!         let base = QueryBase::new(config)?;
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
    ComponentStatus, DispatchMode, QueryResult, QuerySubscriptionResponse,
};
use crate::component_graph::ComponentStatusHandle;
use crate::config::QueryConfig;
use crate::context::QueryRuntimeContext;
// Profiling will be used when implementing performance tracking
use chrono::Utc;

/// Base implementation for common query functionality
pub struct QueryBase {
    /// Query configuration
    pub config: QueryConfig,
    /// Component status handle — wired to graph via initialize().
    status_handle: ComponentStatusHandle,
    /// Dispatchers for sending query results to subscribers
    pub dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>>,
    /// Handle to the query's main task
    pub task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Sender for shutdown signal
    pub shutdown_tx: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl QueryBase {
    /// Create a new QueryBase with the given configuration
    pub fn new(config: QueryConfig) -> Result<Self> {
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

        let status_handle = ComponentStatusHandle::new(&config.id);

        Ok(Self {
            config,
            status_handle,
            dispatchers: Arc::new(RwLock::new(dispatchers)),
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
        })
    }

    /// Returns a clone of the [`ComponentStatusHandle`] for use in spawned tasks.
    pub fn status_handle(&self) -> ComponentStatusHandle {
        self.status_handle.clone()
    }

    /// Initialize the query base with runtime context.
    ///
    /// Wires the status handle to the component graph update channel,
    /// following the same pattern as `SourceBase::initialize()` and
    /// `ReactionBase::initialize()`.
    pub async fn initialize(&self, context: QueryRuntimeContext) {
        self.status_handle.wire(context.update_tx).await;
    }

    /// Set the component's status — updates local state AND notifies the graph.
    ///
    /// This is the single canonical way to change a query's status.
    pub async fn set_status(&self, status: ComponentStatus, message: Option<String>) {
        self.status_handle.set_status(status, message).await;
    }

    /// Get the current status.
    pub async fn get_status(&self) -> ComponentStatus {
        self.status_handle.get_status().await
    }
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

        // Fan out to all dispatchers concurrently so one slow reaction
        // cannot block delivery to the others.
        let dispatchers = self.dispatchers.read().await;
        if dispatchers.len() <= 1 {
            // Fast path: single or no dispatcher, no need to spawn tasks
            for dispatcher in dispatchers.iter() {
                if let Err(e) = dispatcher.dispatch_change(arc_result.clone()).await {
                    debug!("[{}] Failed to dispatch result: {}", self.config.id, e);
                }
            }
        } else {
            // Multiple dispatchers: fan out concurrently via join_all.
            // We create the futures while holding the read lock, then await them.
            // The futures borrow the dispatchers so the lock must stay held,
            // but that's fine — no writes happen during dispatch.
            let futures: Vec<_> = dispatchers
                .iter()
                .map(|d| d.dispatch_change(arc_result.clone()))
                .collect();
            let query_id = &self.config.id;
            for result in futures::future::join_all(futures).await {
                if let Err(e) = result {
                    debug!("[{query_id}] Failed to dispatch result: {e}");
                }
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

    /// Handle common stop functionality.
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

        self.set_status(
            ComponentStatus::Stopped,
            Some(format!("Query '{}' stopped", self.config.id)),
        )
        .await;
        info!("Query '{}' stopped", self.config.id);
        Ok(())
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// Helper to build a minimal QueryConfig for tests.
    fn test_config(id: &str, mode: Option<DispatchMode>) -> QueryConfig {
        QueryConfig {
            id: id.to_string(),
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
            dispatch_mode: mode,
            storage_backend: None,
        }
    }

    // =============================================================================
    // Construction Tests
    // =============================================================================

    #[tokio::test]
    async fn test_new_initial_status_is_stopped() {
        let base = QueryBase::new(test_config("q1", None)).unwrap();
        assert_eq!(base.get_status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_new_channel_mode_starts_with_no_dispatchers() {
        let base = QueryBase::new(test_config("q1", Some(DispatchMode::Channel))).unwrap();
        let dispatchers = base.dispatchers.read().await;
        assert!(
            dispatchers.is_empty(),
            "Channel mode should start with zero dispatchers"
        );
    }

    #[tokio::test]
    async fn test_new_broadcast_mode_starts_with_one_dispatcher() {
        let base = QueryBase::new(test_config("q1", Some(DispatchMode::Broadcast))).unwrap();
        let dispatchers = base.dispatchers.read().await;
        assert_eq!(
            dispatchers.len(),
            1,
            "Broadcast mode should start with one dispatcher"
        );
    }

    #[tokio::test]
    async fn test_new_default_dispatch_mode_is_channel() {
        // dispatch_mode = None should behave like Channel (no initial dispatchers)
        let base = QueryBase::new(test_config("q1", None)).unwrap();
        let dispatchers = base.dispatchers.read().await;
        assert!(dispatchers.is_empty());
    }

    // =============================================================================
    // Status Handle Tests
    // =============================================================================

    #[tokio::test]
    async fn test_status_handle_returns_handle() {
        let base = QueryBase::new(test_config("q1", None)).unwrap();
        let handle = base.status_handle();
        // The returned handle should reflect the same status as the base
        assert_eq!(handle.get_status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_status_handle_shares_state_with_base() {
        let base = QueryBase::new(test_config("q1", None)).unwrap();
        let handle = base.status_handle();

        // Mutate via handle, observe via base
        handle.set_status(ComponentStatus::Running, None).await;
        assert_eq!(base.get_status().await, ComponentStatus::Running);
    }

    // =============================================================================
    // get_status / set_status Tests
    // =============================================================================

    #[tokio::test]
    async fn test_set_and_get_status() {
        let base = QueryBase::new(test_config("q1", None)).unwrap();

        base.set_status(ComponentStatus::Starting, Some("booting".into()))
            .await;
        assert_eq!(base.get_status().await, ComponentStatus::Starting);

        base.set_status(ComponentStatus::Running, None).await;
        assert_eq!(base.get_status().await, ComponentStatus::Running);

        base.set_status(ComponentStatus::Stopped, None).await;
        assert_eq!(base.get_status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_set_status_without_initialization() {
        // set_status should work even if initialize() was never called (no graph)
        let base = QueryBase::new(test_config("q1", None)).unwrap();
        base.set_status(ComponentStatus::Running, None).await;
        assert_eq!(base.get_status().await, ComponentStatus::Running);
    }

    // =============================================================================
    // subscribe() Tests
    // =============================================================================

    #[tokio::test]
    async fn test_subscribe_channel_mode_creates_receiver() {
        let base = QueryBase::new(test_config("q1", Some(DispatchMode::Channel))).unwrap();
        let sub = base.subscribe("reaction-a").await.unwrap();
        assert_eq!(sub.query_id, "q1");

        // A dispatcher should have been added
        let dispatchers = base.dispatchers.read().await;
        assert_eq!(dispatchers.len(), 1);
    }

    #[tokio::test]
    async fn test_subscribe_channel_mode_adds_dispatcher_per_subscription() {
        let base = QueryBase::new(test_config("q1", Some(DispatchMode::Channel))).unwrap();
        let _s1 = base.subscribe("r1").await.unwrap();
        let _s2 = base.subscribe("r2").await.unwrap();
        let _s3 = base.subscribe("r3").await.unwrap();

        let dispatchers = base.dispatchers.read().await;
        assert_eq!(
            dispatchers.len(),
            3,
            "Channel mode should create one dispatcher per subscription"
        );
    }

    #[tokio::test]
    async fn test_subscribe_broadcast_mode_reuses_single_dispatcher() {
        let base = QueryBase::new(test_config("q1", Some(DispatchMode::Broadcast))).unwrap();
        let _s1 = base.subscribe("r1").await.unwrap();
        let _s2 = base.subscribe("r2").await.unwrap();

        let dispatchers = base.dispatchers.read().await;
        assert_eq!(
            dispatchers.len(),
            1,
            "Broadcast mode should reuse the single dispatcher"
        );
    }

    #[tokio::test]
    async fn test_subscribe_returns_correct_query_id() {
        let base = QueryBase::new(test_config("my-query", Some(DispatchMode::Channel))).unwrap();
        let sub = base.subscribe("any-reaction").await.unwrap();
        assert_eq!(sub.query_id, "my-query");
    }

    // =============================================================================
    // stop_common() Tests
    // =============================================================================

    #[tokio::test]
    async fn test_stop_common_sends_shutdown_signal() {
        let base = QueryBase::new(test_config("q1", None)).unwrap();

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        base.set_shutdown_tx(tx).await;

        let shutdown_received = Arc::new(AtomicBool::new(false));
        let flag = shutdown_received.clone();

        let task = tokio::spawn(async move {
            let mut shutdown_rx = rx;
            tokio::select! {
                _ = &mut shutdown_rx => {
                    flag.store(true, Ordering::SeqCst);
                }
            }
        });
        base.set_task_handle(task).await;

        base.stop_common().await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            shutdown_received.load(Ordering::SeqCst),
            "Task should have received shutdown signal"
        );
    }

    #[tokio::test]
    async fn test_stop_common_sets_status_to_stopped() {
        let base = QueryBase::new(test_config("q1", None)).unwrap();

        // Move to Running first
        base.set_status(ComponentStatus::Running, None).await;
        assert_eq!(base.get_status().await, ComponentStatus::Running);

        base.stop_common().await.unwrap();
        assert_eq!(base.get_status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_stop_common_without_task_or_shutdown() {
        // Should succeed gracefully when nothing has been set up
        let base = QueryBase::new(test_config("q1", None)).unwrap();
        let result = base.stop_common().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_stop_common_graceful_shutdown_timing() {
        let base = QueryBase::new(test_config("q1", None)).unwrap();

        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        base.set_shutdown_tx(tx).await;

        let task = tokio::spawn(async move {
            let mut shutdown_rx = rx;
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => { break; }
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                }
            }
        });
        base.set_task_handle(task).await;

        let start = std::time::Instant::now();
        base.stop_common().await.unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "Shutdown took {elapsed:?}, expected < 500ms"
        );
    }

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

        let base = QueryBase::new(config).unwrap();

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

        let base = QueryBase::new(config).unwrap();

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
