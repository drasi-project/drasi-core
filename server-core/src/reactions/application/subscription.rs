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

/// Options for configuring result subscriptions
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
    /// Create new subscription options
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the buffer size
    pub fn with_buffer_size(mut self, size: usize) -> Self {
        self.buffer_size = size;
        self
    }

    /// Filter by specific query names
    pub fn with_query_filter(mut self, queries: Vec<String>) -> Self {
        self.query_filter = queries;
        self
    }

    /// Set a timeout for receiving results
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Enable batching with specified size
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = Some(size);
        self
    }
}

/// Subscription handle for receiving query results
pub struct Subscription {
    rx: mpsc::Receiver<QueryResult>,
    options: SubscriptionOptions,
}

impl Subscription {
    pub(crate) fn new(rx: mpsc::Receiver<QueryResult>, options: SubscriptionOptions) -> Self {
        Self { rx, options }
    }

    /// Receive the next result
    pub async fn recv(&mut self) -> Option<QueryResult> {
        match self.options.timeout {
            Some(timeout) => tokio::time::timeout(timeout, self.rx.recv()).await.ok()?,
            None => self.rx.recv().await,
        }
    }

    /// Try to receive without blocking
    pub fn try_recv(&mut self) -> Option<QueryResult> {
        self.rx.try_recv().ok()
    }

    /// Receive a batch of results
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

    /// Convert to an async iterator
    pub fn into_stream(self) -> SubscriptionStream {
        SubscriptionStream { subscription: self }
    }
}

/// Async stream for iteration
pub struct SubscriptionStream {
    subscription: Subscription,
}

impl SubscriptionStream {
    pub async fn next(&mut self) -> Option<QueryResult> {
        self.subscription.recv().await
    }
}
