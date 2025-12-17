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

use chrono::DateTime;
use drasi_core::interface::FutureQueue;
use drasi_core::models::SourceChange;
use log::{debug, error, info, warn};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};

use crate::channels::{ComponentEventSender, DispatchMode, SourceEvent, SourceEventWrapper};
use crate::ComponentStatus;

/// Internal source ID for the future queue source (used for lifecycle management only)
pub const FUTURE_QUEUE_SOURCE_ID: &str = "__future_queue__";

/// Status of the future queue source
#[derive(Debug, Clone, PartialEq)]
enum FutureQueueSourceStatus {
    Stopped,
    Running,
    Stopping,
}

/// A virtual source that polls the FutureQueue and emits due elements as source events.
/// Unlike regular sources, this source emits events with varying source_ids preserved
/// from the FutureElementRef, allowing temporal events to be properly attributed to
/// their originating sources.
pub struct FutureQueueSource {
    /// The future queue to poll
    future_queue: Arc<dyn FutureQueue>,
    /// Current status of the source
    status: Arc<RwLock<FutureQueueSourceStatus>>,
    /// Task handle for the polling loop
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Query ID for logging
    query_id: String,
    /// Dispatcher for sending events to subscribers
    dispatcher: Arc<RwLock<Option<Box<dyn crate::channels::ChangeDispatcher<SourceEventWrapper>>>>>,
}

impl FutureQueueSource {
    /// Create a new FutureQueueSource
    ///
    /// # Arguments
    /// * `future_queue` - The future queue to poll for due elements
    /// * `query_id` - The query ID for logging and context
    pub fn new(future_queue: Arc<dyn FutureQueue>, query_id: String) -> Self {
        Self {
            future_queue,
            status: Arc::new(RwLock::new(FutureQueueSourceStatus::Stopped)),
            task_handle: Arc::new(RwLock::new(None)),
            query_id,
            dispatcher: Arc::new(RwLock::new(None)),
        }
    }

    /// Start the future queue polling task (internal implementation)
    async fn start_internal(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut status = self.status.write().await;
        if *status == FutureQueueSourceStatus::Running {
            return Err("FutureQueueSource is already running".into());
        }

        info!("Starting FutureQueueSource for query '{}'", self.query_id);

        // Create dispatcher internally
        let dispatcher = crate::channels::ChannelChangeDispatcher::<SourceEventWrapper>::new(1000);
        *self.dispatcher.write().await = Some(Box::new(dispatcher));

        *status = FutureQueueSourceStatus::Running;
        drop(status);

        // Spawn the polling task
        let future_queue = self.future_queue.clone();
        let status_clone = self.status.clone();
        let query_id = self.query_id.clone();
        let dispatcher_clone = self.dispatcher.clone();

        let handle = tokio::spawn(async move {
            debug!("FutureQueueSource polling task started for query '{query_id}'");

            loop {
                // Check if we should stop
                {
                    let status = status_clone.read().await;
                    if *status != FutureQueueSourceStatus::Running {
                        info!("FutureQueueSource polling task stopping for query '{query_id}'");
                        break;
                    }
                }

                // Peek at the next due time
                let next_due_time = match future_queue.peek_due_time().await {
                    Ok(Some(due_time)) => due_time,
                    Ok(None) => {
                        // No items in queue, sleep for a bit and check again
                        sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    Err(e) => {
                        error!(
                            "FutureQueueSource failed to peek due time for query '{query_id}': {e}"
                        );
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };

                // Calculate how long to wait
                let now = Self::now();
                if next_due_time > now {
                    let wait_ms = next_due_time - now;
                    // Cap the wait time at 5 seconds to allow for periodic status checks
                    let wait_ms = wait_ms.min(5000);
                    sleep(Duration::from_millis(wait_ms)).await;
                    continue;
                }

                // Pop the due element
                let future_ref = match future_queue.pop().await {
                    Ok(Some(future_ref)) => future_ref,
                    Ok(None) => {
                        // Race condition - element was already popped
                        continue;
                    }
                    Err(e) => {
                        error!(
                            "FutureQueueSource failed to pop from queue for query '{query_id}': {e}"
                        );
                        sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };

                debug!(
                    "FutureQueueSource popped due element for query '{}': element_ref={}, due_time={}",
                    query_id, future_ref.element_ref, future_ref.due_time
                );

                // Create a SourceChange::Future event
                let source_change = SourceChange::Future {
                    future_ref: future_ref.clone(),
                };

                // Extract the original source_id from the future_ref
                let source_id = future_ref.element_ref.source_id.to_string();

                // Convert due_time to DateTime
                let timestamp = match DateTime::from_timestamp_millis(
                    future_ref.due_time.try_into().unwrap_or(0),
                ) {
                    Some(dt) => dt,
                    None => {
                        warn!(
                            "FutureQueueSource: Due time {} for element {} is out of range, using current time",
                            future_ref.due_time, future_ref.element_ref
                        );
                        chrono::Utc::now()
                    }
                };

                // Create the event wrapper with the ORIGINAL source_id
                let event_wrapper = SourceEventWrapper::new(
                    source_id.clone(),
                    SourceEvent::Change(source_change),
                    timestamp,
                );

                // Dispatch the event
                let dispatcher_guard = dispatcher_clone.read().await;
                if let Some(dispatcher) = dispatcher_guard.as_ref() {
                    if let Err(e) = dispatcher.dispatch_change(Arc::new(event_wrapper)).await {
                        debug!(
                            "FutureQueueSource failed to dispatch event for query '{query_id}': {e}"
                        );
                    }
                } else {
                    warn!("FutureQueueSource: No dispatcher available for query '{query_id}'");
                    break;
                }
            }

            debug!("FutureQueueSource polling task exited for query '{query_id}'");
        });

        *self.task_handle.write().await = Some(handle);

        Ok(())
    }

    /// Stop the future queue polling task (internal implementation)
    async fn stop_internal(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut status = self.status.write().await;
        if *status != FutureQueueSourceStatus::Running {
            return Ok(());
        }

        info!("Stopping FutureQueueSource for query '{}'", self.query_id);
        *status = FutureQueueSourceStatus::Stopping;
        drop(status);

        // Abort the polling task
        let task_handle = self.task_handle.write().await.take();
        if let Some(handle) = task_handle {
            handle.abort();
            let _ = handle.await;
        }

        // Clear the dispatcher
        *self.dispatcher.write().await = None;

        *self.status.write().await = FutureQueueSourceStatus::Stopped;

        info!("FutureQueueSource stopped for query '{}'", self.query_id);

        Ok(())
    }

    /// Get current timestamp in milliseconds since epoch
    fn now() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// Subscribe to future queue events (for query consumption)
    ///
    /// # Arguments
    /// * `query_id` - ID of the subscribing query (unused, kept for API compatibility)
    /// * `enable_bootstrap` - Whether to request initial data (unused, FutureQueueSource doesn't support bootstrap)
    /// * `node_labels` - Node labels the query is interested in (unused)
    /// * `relation_labels` - Relation labels the query is interested in (unused)
    pub async fn subscribe(
        &self,
        _query_id: String,
        _enable_bootstrap: bool,
        _node_labels: Vec<String>,
        _relation_labels: Vec<String>,
    ) -> Result<crate::channels::SubscriptionResponse, Box<dyn std::error::Error + Send + Sync>>
    {
        let status = self.status.read().await;
        if *status != FutureQueueSourceStatus::Running {
            return Err("FutureQueueSource is not running".into());
        }

        // Get the dispatcher that was created during start()
        let dispatcher_guard = self.dispatcher.read().await;
        let dispatcher = dispatcher_guard
            .as_ref()
            .ok_or("FutureQueueSource dispatcher not initialized")?;

        let receiver = dispatcher.create_receiver().await.map_err(
            |e| -> Box<dyn std::error::Error + Send + Sync> {
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                ))
            },
        )?;

        Ok(crate::channels::SubscriptionResponse {
            query_id: self.query_id.clone(),
            source_id: FUTURE_QUEUE_SOURCE_ID.to_string(),
            receiver,
            bootstrap_receiver: None,
        })
    }

    /// Get the dispatch mode (always Channel for future queue source)
    pub fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }
}

// Implement the Source trait for FutureQueueSource
#[async_trait::async_trait]
impl crate::plugin_core::Source for FutureQueueSource {
    fn id(&self) -> &str {
        FUTURE_QUEUE_SOURCE_ID
    }

    fn type_name(&self) -> &str {
        "future-queue"
    }

    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        let mut props = std::collections::HashMap::new();
        props.insert("query_id".to_string(), serde_json::json!(self.query_id));
        props
    }

    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }

    fn auto_start(&self) -> bool {
        false // FutureQueueSource is managed by the query, not auto-started
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.start_internal()
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.stop_internal()
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn status(&self) -> ComponentStatus {
        let status = self.status.read().await;
        match *status {
            FutureQueueSourceStatus::Running => ComponentStatus::Running,
            FutureQueueSourceStatus::Stopped => ComponentStatus::Stopped,
            FutureQueueSourceStatus::Stopping => ComponentStatus::Stopped,
        }
    }

    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> anyhow::Result<crate::channels::SubscriptionResponse> {
        self.subscribe(query_id, enable_bootstrap, node_labels, relation_labels)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn inject_event_tx(&self, _tx: ComponentEventSender) {
        // FutureQueueSource doesn't emit lifecycle events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::in_memory_index::in_memory_future_queue::InMemoryFutureQueue;
    use drasi_core::interface::PushType;
    use drasi_core::models::{ElementReference, ElementTimestamp};

    #[tokio::test]
    async fn test_future_queue_source_lifecycle() {
        use crate::plugin_core::Source;

        let future_queue = Arc::new(InMemoryFutureQueue::new());
        let source = FutureQueueSource::new(future_queue, "test-query".to_string());

        // Initially stopped
        assert_eq!(
            *source.status.read().await,
            FutureQueueSourceStatus::Stopped
        );

        // Start the source
        source.start().await.unwrap();
        assert_eq!(
            *source.status.read().await,
            FutureQueueSourceStatus::Running
        );

        // Stop the source
        source.stop().await.unwrap();
        assert_eq!(
            *source.status.read().await,
            FutureQueueSourceStatus::Stopped
        );
    }

    #[tokio::test]
    async fn test_future_queue_source_emits_events() {
        use crate::plugin_core::Source;

        let future_queue = Arc::new(InMemoryFutureQueue::new());
        let source = FutureQueueSource::new(future_queue.clone(), "test-query".to_string());

        // Add an element that's already due
        let element_ref = ElementReference::new("test-source", "test-element");
        let now: ElementTimestamp = FutureQueueSource::now();
        let past = now.saturating_sub(1000); // 1 second ago

        future_queue
            .push(
                PushType::Always,
                0,
                1234,
                &element_ref,
                past,
                past, // due time in the past
            )
            .await
            .unwrap();

        // Start the source
        source.start().await.unwrap();

        // Subscribe to get events
        let response = source
            .subscribe("test-query".to_string(), false, vec![], vec![])
            .await
            .unwrap();
        let mut receiver = response.receiver;

        // Wait for the event
        let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("Timeout waiting for event")
            .expect("Channel closed unexpectedly");

        // Verify the event
        assert_eq!(event.source_id, "test-source"); // Source ID preserved!
        match &event.event {
            SourceEvent::Change(SourceChange::Future { future_ref }) => {
                assert_eq!(future_ref.element_ref, element_ref);
            }
            _ => panic!("Expected Future change event"),
        }

        source.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_future_queue_source_waits_for_due_time() {
        use crate::plugin_core::Source;

        let future_queue = Arc::new(InMemoryFutureQueue::new());
        let source = FutureQueueSource::new(future_queue.clone(), "test-query".to_string());

        // Add an element that's due in 500ms
        let element_ref = ElementReference::new("test-source", "test-element");
        let now: ElementTimestamp = FutureQueueSource::now();
        let future_time = now + 500;

        future_queue
            .push(PushType::Always, 0, 1234, &element_ref, now, future_time)
            .await
            .unwrap();

        let start = tokio::time::Instant::now();

        // Start the source
        source.start().await.unwrap();

        // Subscribe to get events
        let response = source
            .subscribe("test-query".to_string(), false, vec![], vec![])
            .await
            .unwrap();
        let mut receiver = response.receiver;

        // Wait for the event
        let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("Timeout waiting for event")
            .expect("Channel closed unexpectedly");

        let elapsed = start.elapsed();

        // Should have waited at least 400ms (with some tolerance)
        assert!(
            elapsed >= Duration::from_millis(400),
            "Expected to wait at least 400ms, but only waited {elapsed:?}"
        );

        // Verify the event
        assert_eq!(event.source_id, "test-source");

        source.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_future_queue_source_preserves_source_id() {
        use crate::plugin_core::Source;

        let future_queue = Arc::new(InMemoryFutureQueue::new());
        let source = FutureQueueSource::new(future_queue.clone(), "test-query".to_string());

        // Add elements from different sources
        let now: ElementTimestamp = FutureQueueSource::now();
        let past = now.saturating_sub(1000);

        let sources = vec!["source-a", "source-b", "source-c"];
        for source_id in &sources {
            let element_ref = ElementReference::new(source_id, "element");
            future_queue
                .push(PushType::Always, 0, 1234, &element_ref, past, past)
                .await
                .unwrap();
        }

        // Start the source
        source.start().await.unwrap();

        // Subscribe to get events
        let response = source
            .subscribe("test-query".to_string(), false, vec![], vec![])
            .await
            .unwrap();
        let mut receiver = response.receiver;

        // Collect events
        let mut received_source_ids = vec![];
        for _ in 0..3 {
            let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .expect("Timeout waiting for event")
                .expect("Channel closed unexpectedly");
            received_source_ids.push(event.source_id.to_string());
        }

        // Verify all source IDs were preserved
        received_source_ids.sort();
        let mut expected = sources.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        expected.sort();
        assert_eq!(received_source_ids, expected);

        source.stop().await.unwrap();
    }
}
