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

pub use super::config::ApplicationReactionConfig;
use crate::subscription::{Subscription, SubscriptionOptions};

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info};
use std::sync::Arc;
// RecvError no longer needed with trait-based receivers
use tokio::sync::{mpsc, RwLock};

use drasi_lib::channels::{ComponentEventSender, ComponentStatus, QueryResult};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::{QuerySubscriber, Reaction};
use std::collections::HashMap;

/// Handle for programmatic consumption of query results from an Application Reaction
///
/// `ApplicationReactionHandle` provides APIs for receiving continuous query results in your
/// application code. It supports multiple consumption patterns: callback-based, async streams,
/// and flexible subscriptions with filtering and buffering options.
///
/// # Usage Pattern
///
/// 1. Get the handle from `DrasiServerCore::reaction_handle()`
/// 2. Choose a consumption pattern:
///    - **Subscription** (recommended): Use `subscribe_with_options()` for flexible result consumption
///    - **Async Stream**: Use `as_stream()` for async iteration
///    - **Callback**: Use `subscribe()` or `subscribe_filtered()` for callback-based processing
///
/// # Important: Single Consumer
///
/// Each handle can only be used once. The underlying receiver is taken on first use and cannot
/// be reused. If you need multiple consumers, create multiple application reactions.
///
/// # Thread Safety
///
/// `ApplicationReactionHandle` is `Clone` but the underlying receiver can only be taken once.
/// Cloning the handle shares the same receiver, so only one clone can successfully take it.
///
/// # Examples
///
/// ## Basic Subscription (Recommended)
///
/// ```ignore
/// // This example shows how to use ApplicationReactionHandle with DrasiLib
/// let handle = core.reaction_handle("results").await?;
///
/// // Create a subscription with default options
/// let mut subscription = handle.subscribe_with_options(
///     SubscriptionOptions::default()
/// ).await?;
///
/// // Receive results one at a time
/// while let Some(result) = subscription.recv().await {
///     println!("Query: {}, Results: {}", result.query_id, result.results.len());
///     for row in result.results {
///         println!("  {:?}", row);
///     }
/// }
/// ```
///
/// ## Async Stream Pattern
///
/// ```ignore
/// let handle = core.reaction_handle("results").await?;
///
/// // Get as async stream
/// if let Some(mut stream) = handle.as_stream().await {
///     while let Some(result) = stream.next().await {
///         println!("Received: {:?}", result);
///     }
/// }
/// ```
///
/// ## Callback Pattern
///
/// ```ignore
/// let handle = core.reaction_handle("results").await?;
///
/// // Process results with a callback (spawns background task)
/// handle.subscribe(|result| {
///     println!("Query: {}, Result count: {}", result.query_id, result.results.len());
/// }).await?;
///
/// // Keep main task alive while callback processes results
/// tokio::time::sleep(Duration::from_secs(60)).await;
/// ```
///
/// ## Filtered Subscription
///
/// ```ignore
/// let handle = core.reaction_handle("results").await?;
///
/// // Only receive results from specific queries
/// handle.subscribe_filtered(
///     vec!["users".to_string()],  // Filter: only "users" query results
///     |result| {
///         println!("User result: {:?}", result);
///     }
/// ).await?;
/// ```
///
/// ## Subscription with Options
///
/// ```ignore
/// let handle = core.reaction_handle("results").await?;
///
/// // Configure subscription behavior
/// let options = SubscriptionOptions::default()
///     .with_buffer_size(1000)      // Buffer up to 1000 results
///     .with_query_filter(vec!["users".to_string()])  // Filter by query
///     .with_batch_size(10);        // Receive up to 10 at a time
///
/// let mut subscription = handle.subscribe_with_options(options).await?;
///
/// // Receive results in batches
/// loop {
///     let batch = subscription.recv_batch().await;
///     if batch.is_empty() {
///         break;
///     }
///     println!("Received batch of {} results", batch.len());
/// }
/// ```
#[derive(Clone)]
pub struct ApplicationReactionHandle {
    rx: Arc<RwLock<Option<mpsc::Receiver<QueryResult>>>>,
    reaction_id: String,
}

impl ApplicationReactionHandle {
    /// Take the underlying receiver (low-level API, use with caution)
    ///
    /// This is a low-level method that transfers ownership of the underlying receiver.
    /// Most users should use [`subscribe_with_options()`](Self::subscribe_with_options),
    /// [`as_stream()`](Self::as_stream), or [`subscribe()`](Self::subscribe) instead.
    ///
    /// # Returns
    ///
    /// * `Some(receiver)` - The first time this method is called
    /// * `None` - If the receiver has already been taken
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe but can only successfully complete once across all clones.
    pub async fn take_receiver(&self) -> Option<mpsc::Receiver<QueryResult>> {
        self.rx.write().await.take()
    }

    /// Subscribe to query results with a callback function
    ///
    /// Spawns a background task that processes query results by calling the provided callback
    /// for each result. The callback runs in a separate task, so your main code continues
    /// executing immediately.
    ///
    /// **Note**: This method takes ownership of the receiver, so it can only be called once.
    ///
    /// # Arguments
    ///
    /// * `callback` - Function to call for each query result (must be `Send + 'static`)
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the subscription was set up successfully.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The receiver has already been taken (`anyhow::Error`)
    ///
    /// # Thread Safety
    ///
    /// The callback is invoked in a background task and must be `Send + 'static`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let handle = core.reaction_handle("results").await?;
    ///
    /// handle.subscribe(|result| {
    ///     println!("Received {} results from query {}",
    ///              result.results.len(),
    ///              result.query_id);
    /// }).await?;
    ///
    /// // Callback continues processing in background
    /// ```
    pub async fn subscribe<F>(&self, mut callback: F) -> Result<()>
    where
        F: FnMut(QueryResult) + Send + 'static,
    {
        if let Some(mut rx) = self.take_receiver().await {
            tokio::spawn(async move {
                while let Some(result) = rx.recv().await {
                    callback(result);
                }
            });
            Ok(())
        } else {
            Err(anyhow::anyhow!("Receiver already taken"))
        }
    }

    /// Subscribe to query results with filtering by query ID
    ///
    /// Similar to [`subscribe()`](Self::subscribe) but only invokes the callback for results
    /// from specific queries. Useful when a reaction subscribes to multiple queries but you
    /// only want to process results from some of them.
    ///
    /// **Note**: This method takes ownership of the receiver, so it can only be called once.
    ///
    /// # Arguments
    ///
    /// * `query_filter` - List of query IDs to receive results from (empty = receive all)
    /// * `callback` - Function to call for each matching query result
    ///
    /// # Errors
    ///
    /// Returns an error if the receiver has already been taken.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let handle = core.reaction_handle("results").await?;
    ///
    /// // Only process results from "users" query
    /// handle.subscribe_filtered(
    ///     vec!["users".to_string()],
    ///     |result| {
    ///         println!("User query result: {:?}", result);
    ///     }
    /// ).await?;
    /// ```
    pub async fn subscribe_filtered<F>(
        &self,
        query_filter: Vec<String>,
        mut callback: F,
    ) -> Result<()>
    where
        F: FnMut(QueryResult) + Send + 'static,
    {
        if let Some(mut rx) = self.take_receiver().await {
            tokio::spawn(async move {
                while let Some(result) = rx.recv().await {
                    if query_filter.is_empty() || query_filter.contains(&result.query_id) {
                        callback(result);
                    }
                }
            });
            Ok(())
        } else {
            Err(anyhow::anyhow!("Receiver already taken"))
        }
    }

    /// Get query results as an async stream
    ///
    /// Converts the result receiver into a [`ResultStream`] for async iteration. This is
    /// useful when you want to process results in a loop with async/await syntax.
    ///
    /// **Note**: This method takes ownership of the receiver, so it can only be called once.
    ///
    /// # Returns
    ///
    /// * `Some(ResultStream)` - The first time this method is called
    /// * `None` - If the receiver has already been taken
    ///
    /// # Thread Safety
    ///
    /// The returned stream is not `Send` and should be used within a single task.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let handle = core.reaction_handle("results").await?;
    ///
    /// if let Some(mut stream) = handle.as_stream().await {
    ///     while let Some(result) = stream.next().await {
    ///         println!("Query {}: {} results",
    ///                  result.query_id,
    ///                  result.results.len());
    ///     }
    /// }
    /// ```
    pub async fn as_stream(&self) -> Option<ResultStream> {
        self.take_receiver().await.map(ResultStream::new)
    }

    /// Create a flexible subscription with custom options (recommended)
    ///
    /// Creates a [`Subscription`] with configurable behavior including buffering, filtering,
    /// and batch processing. This is the most flexible way to consume query results.
    ///
    /// **Note**: This method takes ownership of the receiver, so it can only be called once.
    ///
    /// # Arguments
    ///
    /// * `options` - Configuration for the subscription behavior
    ///
    /// # Returns
    ///
    /// Returns `Ok(Subscription)` if successful.
    ///
    /// # Errors
    ///
    /// Returns an error if the receiver has already been taken.
    ///
    /// # Thread Safety
    ///
    /// The returned subscription can be moved across threads.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let handle = core.reaction_handle("results").await?;
    ///
    /// // Configure subscription
    /// let options = SubscriptionOptions::default()
    ///     .with_buffer_size(1000)
    ///     .with_batch_size(10);
    ///
    /// let mut subscription = handle.subscribe_with_options(options).await?;
    ///
    /// // Receive one result at a time
    /// while let Some(result) = subscription.recv().await {
    ///     println!("Result: {:?}", result);
    /// }
    /// ```
    pub async fn subscribe_with_options(
        &self,
        options: SubscriptionOptions,
    ) -> Result<Subscription> {
        if let Some(rx) = self.take_receiver().await {
            Ok(Subscription::new(rx, options))
        } else {
            Err(anyhow::anyhow!("Receiver already taken"))
        }
    }

    /// Get the reaction ID that this handle is connected to
    ///
    /// Returns the unique identifier of the application reaction that this handle receives
    /// results from.
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let handle = core.reaction_handle("results").await?;
    /// assert_eq!(handle.reaction_id(), "results");
    /// ```
    pub fn reaction_id(&self) -> &str {
        &self.reaction_id
    }
}

/// Stream wrapper for async iteration over query results
///
/// `ResultStream` provides a simple async iterator interface for processing query results
/// one at a time. It's created by calling [`ApplicationReactionHandle::as_stream()`].
///
/// # Examples
///
/// ```ignore
/// let handle = core.reaction_handle("results").await?;
///
/// if let Some(mut stream) = handle.as_stream().await {
///     while let Some(result) = stream.next().await {
///         println!("Received result from {}", result.query_id);
///     }
/// }
/// ```
pub struct ResultStream {
    rx: mpsc::Receiver<QueryResult>,
}

impl ResultStream {
    fn new(rx: mpsc::Receiver<QueryResult>) -> Self {
        Self { rx }
    }

    /// Receive the next query result (async, blocking)
    ///
    /// Waits asynchronously for the next result to arrive. Returns `None` when the stream
    /// has ended (reaction stopped or channel closed).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// if let Some(mut stream) = handle.as_stream().await {
    ///     while let Some(result) = stream.next().await {
    ///         // Process result
    ///         println!("{:?}", result);
    ///     }
    /// }
    /// ```
    pub async fn next(&mut self) -> Option<QueryResult> {
        self.rx.recv().await
    }

    /// Try to receive the next result without blocking
    ///
    /// Returns immediately with `Some(result)` if a result is available, or `None` if
    /// the stream is empty or has ended.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// if let Some(mut stream) = handle.as_stream().await {
    ///     // Non-blocking check for results
    ///     if let Some(result) = stream.try_next() {
    ///         println!("Got result immediately: {:?}", result);
    ///     } else {
    ///         println!("No results available right now");
    ///     }
    /// }
    /// ```
    pub fn try_next(&mut self) -> Option<QueryResult> {
        self.rx.try_recv().ok()
    }
}

use super::ApplicationReactionBuilder;

/// A reaction that sends query results to the host application
pub struct ApplicationReaction {
    base: ReactionBase,
    app_tx: mpsc::Sender<QueryResult>,
}

impl ApplicationReaction {
    /// Create a builder for ApplicationReaction
    pub fn builder(id: impl Into<String>) -> ApplicationReactionBuilder {
        ApplicationReactionBuilder::new(id)
    }

    /// Create a new application reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(id: impl Into<String>, queries: Vec<String>) -> (Self, ApplicationReactionHandle) {
        Self::create_internal(id.into(), queries, None, true)
    }

    /// Create from builder (internal method)
    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> (Self, ApplicationReactionHandle) {
        Self::create_internal(id, queries, priority_queue_capacity, auto_start)
    }

    /// Internal constructor
    fn create_internal(
        id: String,
        queries: Vec<String>,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> (Self, ApplicationReactionHandle) {
        let (app_tx, app_rx) = mpsc::channel(1000);

        let handle = ApplicationReactionHandle {
            rx: Arc::new(RwLock::new(Some(app_rx))),
            reaction_id: id.clone(),
        };

        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        let reaction = Self {
            base: ReactionBase::new(params),
            app_tx,
        };

        (reaction, handle)
    }
}

#[async_trait]
impl Reaction for ApplicationReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "application"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn inject_query_subscriber(&self, query_subscriber: Arc<dyn QuerySubscriber>) {
        self.base.inject_query_subscriber(query_subscriber).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("Reaction", &self.base.id);

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting application reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        // QuerySubscriber was injected via inject_query_subscriber() when reaction was added
        self.base.subscribe_to_queries().await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Application reaction started".to_string()),
            )
            .await?;

        // Create shutdown channel for graceful termination
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // Spawn processing task to dequeue and process results in timestamp order
        let priority_queue = self.base.priority_queue.clone();
        let reaction_name = self.base.id.clone();
        let app_tx = self.app_tx.clone();
        let query_filter = self.base.queries.clone();

        let processing_task = tokio::spawn(async move {
            info!("ApplicationReaction '{reaction_name}' result processor started");

            loop {
                // Use select to wait for either a result OR shutdown signal
                let query_result_arc = tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                        break;
                    }

                    result = priority_queue.dequeue() => result,
                };

                // Clone to get owned QueryResult
                let query_result = (*query_result_arc).clone();

                // Filter results based on configured queries
                if !query_filter.is_empty() && !query_filter.contains(&query_result.query_id) {
                    continue;
                }

                debug!(
                    "ApplicationReaction '{}' forwarding result from query '{}': {} results",
                    reaction_name,
                    query_result.query_id,
                    query_result.results.len()
                );

                // Forward to application
                if let Err(e) = app_tx.send(query_result).await {
                    error!("Failed to send result to application: {e}");
                    break;
                }
            }
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_task).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Use ReactionBase common stop functionality
        self.base.stop_common().await?;

        // Transition to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Application reaction stopped".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}

#[cfg(test)]
mod tests;
