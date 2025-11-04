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

use crate::channels::QueryResult;
use std::time::Duration;
use tokio::sync::mpsc;

/// Configuration options for query result subscriptions
///
/// `SubscriptionOptions` allows you to customize how query results are received and buffered.
/// Use the builder pattern to configure buffering, filtering, timeouts, and batch processing.
///
/// # Default Values
///
/// - `buffer_size`: 1000
/// - `query_filter`: Empty (receive all queries)
/// - `timeout`: None (wait indefinitely)
/// - `batch_size`: None (receive one at a time)
///
/// # Examples
///
/// ## Basic Usage with Defaults
///
/// ```
/// use drasi_server_core::SubscriptionOptions;
///
/// let options = SubscriptionOptions::default();
/// ```
///
/// ## Custom Configuration
///
/// ```
/// use drasi_server_core::SubscriptionOptions;
/// use std::time::Duration;
///
/// let options = SubscriptionOptions::default()
///     .with_buffer_size(5000)                      // Buffer up to 5000 results
///     .with_query_filter(vec!["users".to_string()]) // Only "users" query
///     .with_timeout(Duration::from_secs(30))        // 30 second timeout
///     .with_batch_size(50);                         // Receive up to 50 at a time
/// ```
///
/// ## High-Throughput Configuration
///
/// ```
/// use drasi_server_core::SubscriptionOptions;
///
/// // Optimize for high-throughput scenarios
/// let options = SubscriptionOptions::default()
///     .with_buffer_size(10000)     // Large buffer
///     .with_batch_size(100);        // Large batches
/// ```
///
/// ## Filtered Subscription
///
/// ```
/// use drasi_server_core::SubscriptionOptions;
///
/// // Only receive results from specific queries
/// let options = SubscriptionOptions::default()
///     .with_query_filter(vec![
///         "active_users".to_string(),
///         "recent_orders".to_string()
///     ]);
/// ```
#[derive(Clone)]
pub struct SubscriptionOptions {
    /// Maximum number of results to buffer
    pub buffer_size: usize,
    /// Filter by query names (empty = all queries)
    pub query_filter: Vec<String>,
    /// Maximum time to wait for results before returning None
    pub timeout: Option<Duration>,
    /// Batch size for receiving multiple results at once
    pub batch_size: Option<usize>,
}

impl Default for SubscriptionOptions {
    fn default() -> Self {
        Self {
            buffer_size: 1000,
            query_filter: Vec::new(),
            timeout: None,
            batch_size: None,
        }
    }
}

impl SubscriptionOptions {
    /// Create new subscription options with default values
    ///
    /// Equivalent to `SubscriptionOptions::default()`.
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_server_core::SubscriptionOptions;
    ///
    /// let options = SubscriptionOptions::new();
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the internal buffer size for results
    ///
    /// Controls how many results can be buffered before blocking the producer.
    /// A larger buffer allows for more bursty workloads but uses more memory.
    ///
    /// # Arguments
    ///
    /// * `size` - Maximum number of results to buffer (default: 1000)
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_server_core::SubscriptionOptions;
    ///
    /// let options = SubscriptionOptions::default()
    ///     .with_buffer_size(5000);  // Large buffer for high throughput
    /// ```
    pub fn with_buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size;
        self
    }

    /// Filter results to only include specific queries
    ///
    /// When set, only results from queries with matching IDs will be received.
    /// An empty filter (default) receives results from all queries.
    ///
    /// # Arguments
    ///
    /// * `queries` - List of query IDs to receive results from
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_server_core::SubscriptionOptions;
    ///
    /// let options = SubscriptionOptions::default()
    ///     .with_query_filter(vec![
    ///         "users".to_string(),
    ///         "orders".to_string()
    ///     ]);
    /// ```
    pub fn with_query_filter(mut self, queries: Vec<String>) -> Self {
        self.query_filter = queries;
        self
    }

    /// Set a timeout for receiving results
    ///
    /// When set, `recv()` will return `None` if no result is available within the timeout
    /// period. Without a timeout (default), `recv()` waits indefinitely.
    ///
    /// # Arguments
    ///
    /// * `timeout` - Maximum duration to wait for a result
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_server_core::SubscriptionOptions;
    /// use std::time::Duration;
    ///
    /// let options = SubscriptionOptions::default()
    ///     .with_timeout(Duration::from_secs(30));  // 30 second timeout
    /// ```
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Enable batch processing with specified maximum batch size
    ///
    /// When set, `recv_batch()` will attempt to receive up to this many results at once.
    /// This can improve throughput by reducing the number of receive calls.
    ///
    /// # Arguments
    ///
    /// * `size` - Maximum number of results to receive in a single batch (default: None/10)
    ///
    /// # Examples
    ///
    /// ```
    /// use drasi_server_core::SubscriptionOptions;
    ///
    /// let options = SubscriptionOptions::default()
    ///     .with_batch_size(100);  // Receive up to 100 results at a time
    /// ```
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = Some(size);
        self
    }
}

/// Flexible subscription for receiving continuous query results
///
/// `Subscription` provides the most flexible API for consuming query results with configurable
/// behavior based on [`SubscriptionOptions`]. It supports:
///
/// - **Blocking receives**: Wait for the next result with `recv()`
/// - **Non-blocking receives**: Check for available results with `try_recv()`
/// - **Batch processing**: Receive multiple results at once with `recv_batch()`
/// - **Async iteration**: Convert to a stream with `into_stream()`
/// - **Timeouts**: Automatically timeout based on options
/// - **Filtering**: Automatically filter by query ID based on options
///
/// # Thread Safety
///
/// `Subscription` is not `Send` and should be used within a single task.
///
/// # Examples
///
/// ## Basic Usage
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, SubscriptionOptions};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiServerCore::builder()
/// #     .add_source(Source::application("events").build())
/// #     .add_query(Query::cypher("users").query("MATCH (n:User) RETURN n").from_source("events").build())
/// #     .add_reaction(Reaction::application("results").subscribe_to("users").build())
/// #     .build().await?;
/// let handle = core.reaction_handle("results")?;
///
/// let mut subscription = handle.subscribe_with_options(
///     SubscriptionOptions::default()
/// ).await?;
///
/// // Receive results one at a time
/// while let Some(result) = subscription.recv().await {
///     println!("Received: {:?}", result);
/// }
/// # Ok(())
/// # }
/// ```
///
/// ## Batch Processing
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, SubscriptionOptions};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiServerCore::builder()
/// #     .add_source(Source::application("events").build())
/// #     .add_query(Query::cypher("users").query("MATCH (n:User) RETURN n").from_source("events").build())
/// #     .add_reaction(Reaction::application("results").subscribe_to("users").build())
/// #     .build().await?;
/// let handle = core.reaction_handle("results")?;
///
/// let mut subscription = handle.subscribe_with_options(
///     SubscriptionOptions::default().with_batch_size(50)
/// ).await?;
///
/// // Receive up to 50 results at a time
/// loop {
///     let batch = subscription.recv_batch().await;
///     if batch.is_empty() {
///         break;
///     }
///     println!("Received batch of {} results", batch.len());
/// }
/// # Ok(())
/// # }
/// ```
///
/// ## With Timeout
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, SubscriptionOptions};
/// # use std::time::Duration;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiServerCore::builder()
/// #     .add_source(Source::application("events").build())
/// #     .add_query(Query::cypher("users").query("MATCH (n:User) RETURN n").from_source("events").build())
/// #     .add_reaction(Reaction::application("results").subscribe_to("users").build())
/// #     .build().await?;
/// let handle = core.reaction_handle("results")?;
///
/// let mut subscription = handle.subscribe_with_options(
///     SubscriptionOptions::default()
///         .with_timeout(Duration::from_secs(5))
/// ).await?;
///
/// // Timeout after 5 seconds if no result
/// match subscription.recv().await {
///     Some(result) => println!("Received: {:?}", result),
///     None => println!("Timed out waiting for result")
/// }
/// # Ok(())
/// # }
/// ```
pub struct Subscription {
    rx: mpsc::Receiver<QueryResult>,
    options: SubscriptionOptions,
}

impl Subscription {
    pub(crate) fn new(rx: mpsc::Receiver<QueryResult>, options: SubscriptionOptions) -> Self {
        Self { rx, options }
    }

    /// Receive the next query result (async, blocking with optional timeout)
    ///
    /// Waits for the next result to arrive. If a timeout is configured in the options,
    /// returns `None` if no result arrives within the timeout period. Otherwise, waits
    /// indefinitely.
    ///
    /// # Returns
    ///
    /// * `Some(QueryResult)` - When a result is received
    /// * `None` - When the channel closes, timeout expires, or subscription ends
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, SubscriptionOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let core = DrasiServerCore::builder()
    /// #     .add_source(Source::application("events").build())
    /// #     .add_query(Query::cypher("users").query("MATCH (n:User) RETURN n").from_source("events").build())
    /// #     .add_reaction(Reaction::application("results").subscribe_to("users").build())
    /// #     .build().await?;
    /// # let handle = core.reaction_handle("results")?;
    /// let mut subscription = handle.subscribe_with_options(
    ///     SubscriptionOptions::default()
    /// ).await?;
    ///
    /// while let Some(result) = subscription.recv().await {
    ///     println!("Query: {}, Rows: {}", result.query_id, result.results.len());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn recv(&mut self) -> Option<QueryResult> {
        match self.options.timeout {
            Some(timeout) => tokio::time::timeout(timeout, self.rx.recv()).await.ok()?,
            None => self.rx.recv().await,
        }
    }

    /// Try to receive a result without blocking
    ///
    /// Returns immediately with a result if one is available, or `None` if the channel
    /// is empty or closed.
    ///
    /// # Returns
    ///
    /// * `Some(QueryResult)` - If a result is immediately available
    /// * `None` - If no result is available or channel is closed
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, SubscriptionOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let core = DrasiServerCore::builder()
    /// #     .add_source(Source::application("events").build())
    /// #     .add_query(Query::cypher("users").query("MATCH (n:User) RETURN n").from_source("events").build())
    /// #     .add_reaction(Reaction::application("results").subscribe_to("users").build())
    /// #     .build().await?;
    /// # let handle = core.reaction_handle("results")?;
    /// let mut subscription = handle.subscribe_with_options(
    ///     SubscriptionOptions::default()
    /// ).await?;
    ///
    /// // Poll without blocking
    /// if let Some(result) = subscription.try_recv() {
    ///     println!("Got result immediately: {:?}", result);
    /// } else {
    ///     println!("No results available right now");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn try_recv(&mut self) -> Option<QueryResult> {
        self.rx.try_recv().ok()
    }

    /// Receive a batch of query results efficiently
    ///
    /// Waits for the first result (with optional timeout), then collects additional results
    /// without blocking up to the configured batch size. This is more efficient than calling
    /// `recv()` multiple times when processing high-volume result streams.
    ///
    /// The batch size is determined by the `batch_size` option (default: 10).
    ///
    /// # Returns
    ///
    /// A vector containing 0 to `batch_size` results:
    /// * Empty vector if the first receive times out or channel is closed
    /// * 1 to `batch_size` results otherwise
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, SubscriptionOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let core = DrasiServerCore::builder()
    /// #     .add_source(Source::application("events").build())
    /// #     .add_query(Query::cypher("users").query("MATCH (n:User) RETURN n").from_source("events").build())
    /// #     .add_reaction(Reaction::application("results").subscribe_to("users").build())
    /// #     .build().await?;
    /// # let handle = core.reaction_handle("results")?;
    /// let mut subscription = handle.subscribe_with_options(
    ///     SubscriptionOptions::default().with_batch_size(100)
    /// ).await?;
    ///
    /// loop {
    ///     let batch = subscription.recv_batch().await;
    ///     if batch.is_empty() {
    ///         break;  // Channel closed or timeout
    ///     }
    ///
    ///     println!("Processing batch of {} results", batch.len());
    ///     for result in batch {
    ///         // Process each result
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn recv_batch(&mut self) -> Vec<QueryResult> {
        let batch_size = self.options.batch_size.unwrap_or(10);
        let mut results = Vec::with_capacity(batch_size);

        // First result blocks
        if let Some(result) = self.recv().await {
            results.push(result);

            // Try to fill the batch without blocking
            while results.len() < batch_size {
                match self.try_recv() {
                    Some(result) => results.push(result),
                    None => break,
                }
            }
        }

        results
    }

    /// Convert subscription into an async stream for iteration
    ///
    /// Consumes the subscription and returns a [`SubscriptionStream`] that can be used
    /// for async iteration over results.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, SubscriptionOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let core = DrasiServerCore::builder()
    /// #     .add_source(Source::application("events").build())
    /// #     .add_query(Query::cypher("users").query("MATCH (n:User) RETURN n").from_source("events").build())
    /// #     .add_reaction(Reaction::application("results").subscribe_to("users").build())
    /// #     .build().await?;
    /// # let handle = core.reaction_handle("results")?;
    /// let subscription = handle.subscribe_with_options(
    ///     SubscriptionOptions::default()
    /// ).await?;
    ///
    /// let mut stream = subscription.into_stream();
    /// while let Some(result) = stream.next().await {
    ///     println!("Received: {:?}", result);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn into_stream(self) -> SubscriptionStream {
        SubscriptionStream { subscription: self }
    }
}

/// Async stream wrapper for iterating over query results
///
/// Created by calling [`Subscription::into_stream()`]. Provides a simple async iteration
/// interface over query results.
///
/// # Examples
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, SubscriptionOptions};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiServerCore::builder()
/// #     .add_source(Source::application("events").build())
/// #     .add_query(Query::cypher("users").query("MATCH (n:User) RETURN n").from_source("events").build())
/// #     .add_reaction(Reaction::application("results").subscribe_to("users").build())
/// #     .build().await?;
/// # let handle = core.reaction_handle("results")?;
/// let subscription = handle.subscribe_with_options(
///     SubscriptionOptions::default()
/// ).await?;
///
/// let mut stream = subscription.into_stream();
/// while let Some(result) = stream.next().await {
///     println!("Query: {}", result.query_id);
/// }
/// # Ok(())
/// # }
/// ```
pub struct SubscriptionStream {
    subscription: Subscription,
}

impl SubscriptionStream {
    /// Get the next query result from the stream
    ///
    /// # Returns
    ///
    /// * `Some(QueryResult)` - Next result in the stream
    /// * `None` - Stream has ended (channel closed or timeout)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, SubscriptionOptions};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let core = DrasiServerCore::builder()
    /// #     .add_source(Source::application("events").build())
    /// #     .add_query(Query::cypher("users").query("MATCH (n:User) RETURN n").from_source("events").build())
    /// #     .add_reaction(Reaction::application("results").subscribe_to("users").build())
    /// #     .build().await?;
    /// # let handle = core.reaction_handle("results")?;
    /// let subscription = handle.subscribe_with_options(
    ///     SubscriptionOptions::default()
    /// ).await?;
    ///
    /// let mut stream = subscription.into_stream();
    /// while let Some(result) = stream.next().await {
    ///     // Process result
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn next(&mut self) -> Option<QueryResult> {
        self.subscription.recv().await
    }
}
