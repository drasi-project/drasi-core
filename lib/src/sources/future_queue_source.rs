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
use log::{debug, error, info, warn};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};

use crate::channels::{
    ChangeDispatcher, ChangeReceiver, ChannelChangeDispatcher, SourceControl, SourceEvent,
    SourceEventWrapper,
};
use tracing::Instrument;

/// Internal source ID for the future queue source (used for lifecycle management only)
pub const FUTURE_QUEUE_SOURCE_ID: &str = "__future_queue__";

/// Status of the future queue source
#[derive(Debug, Clone, PartialEq)]
enum FutureQueueSourceStatus {
    Stopped,
    Running,
    Stopping,
}

/// A peek-only signaler that polls the FutureQueue and dispatches `FuturesDue` control
/// signals when items are due. It never pops — the processor calls
/// `process_due_futures_at(now)`, which checks and pops within a session transaction.
pub struct FutureQueueSource {
    #[cfg(feature = "test-support")]
    pub(crate) test_control: Option<crate::test_support::QueryTestControl>,
    #[cfg(feature = "test-support")]
    test_requests: tokio::sync::Mutex<Option<crate::test_support::WakeReceiver>>,
    #[cfg(test)]
    pub(crate) now_override: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// The future queue to poll
    future_queue: Arc<dyn FutureQueue>,
    /// Current status of the source
    status: Arc<RwLock<FutureQueueSourceStatus>>,
    /// Task handle for the polling loop
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Query ID for logging
    query_id: String,
    /// Dispatcher for sending events to subscribers
    dispatcher: Arc<RwLock<Option<Box<dyn ChangeDispatcher<SourceEventWrapper>>>>>,
}

impl FutureQueueSource {
    #[cfg(test)]
    pub(crate) fn future_queue_for_test(&self) -> Arc<dyn FutureQueue> {
        self.future_queue.clone()
    }

    /// Create a new FutureQueueSource
    pub fn new(future_queue: Arc<dyn FutureQueue>, query_id: String) -> Self {
        Self {
            #[cfg(feature = "test-support")]
            test_control: None,
            #[cfg(feature = "test-support")]
            test_requests: tokio::sync::Mutex::new(None),
            #[cfg(test)]
            now_override: None,
            future_queue,
            status: Arc::new(RwLock::new(FutureQueueSourceStatus::Stopped)),
            task_handle: Arc::new(RwLock::new(None)),
            query_id,
            dispatcher: Arc::new(RwLock::new(None)),
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn with_test_control(
        mut self,
        control: crate::test_support::QueryTestControl,
    ) -> crate::error::Result<Self> {
        *self.test_requests.get_mut() = Some(control.connect()?);
        self.test_control = Some(control);
        Ok(self)
    }

    pub(crate) fn physical_now(&self) -> u64 {
        #[cfg(feature = "test-support")]
        if let Some(control) = &self.test_control {
            return control.now();
        }
        #[cfg(test)]
        if let Some(clock) = &self.now_override {
            return clock.load(std::sync::atomic::Ordering::Acquire);
        }
        Self::now()
    }

    /// Subscribe to future queue signals.
    /// Creates a channel dispatcher and returns its receiver.
    pub async fn subscribe(
        &self,
    ) -> Result<Box<dyn ChangeReceiver<SourceEventWrapper>>, Box<dyn std::error::Error + Send + Sync>>
    {
        let dispatcher = ChannelChangeDispatcher::<SourceEventWrapper>::new(1000);
        let receiver = dispatcher.create_receiver().await.map_err(
            |e| -> Box<dyn std::error::Error + Send + Sync> {
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to create receiver for future queue subscription: {e}"),
                ))
            },
        )?;
        *self.dispatcher.write().await = Some(Box::new(dispatcher));
        Ok(receiver)
    }

    /// Start the future queue polling task
    pub async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut status = self.status.write().await;
        if *status == FutureQueueSourceStatus::Running {
            return Err("FutureQueueSource is already running".into());
        }

        info!("Starting FutureQueueSource for query '{}'", self.query_id);
        *status = FutureQueueSourceStatus::Running;
        drop(status);

        let future_queue = self.future_queue.clone();
        let status_clone = self.status.clone();
        let query_id = self.query_id.clone();
        let dispatcher_clone = self.dispatcher.clone();
        #[cfg(feature = "test-support")]
        let mut test_requests = self.test_requests.lock().await.take();
        #[cfg(feature = "test-support")]
        let test_control = self.test_control.clone();
        #[cfg(test)]
        let now_override = self.now_override.clone();

        let span = tracing::info_span!(
            "future_queue_polling",
            component_id = %query_id,
            component_type = "query"
        );
        let handle = tokio::spawn(
            async move {
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

                    #[cfg(feature = "test-support")]
                    let request_id = if let Some(requests) = test_requests.as_mut() {
                        let Some(id) = requests.recv().await else { break };
                        Some(id)
                    } else {
                        None
                    };
                    #[cfg(feature = "test-support")]
                    let explicit_wake = request_id.map(|request_id| (
                        SourceControl::FuturesDueForTest { request_id }, chrono::Utc::now(),
                    ));
                    #[cfg(not(feature = "test-support"))]
                    let explicit_wake: Option<(SourceControl, chrono::DateTime<chrono::Utc>)> = None;

                    let (control, timestamp) = if let Some(wake) = explicit_wake {
                        wake
                    } else {
                        // Peek at the next due time
                        let next_due_time = match future_queue.peek_due_time().await {
                            Ok(Some(due_time)) => due_time,
                            Ok(None) => {
                                // No items in queue, sleep and check again
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
                        #[cfg(test)]
                        let now = now_override.as_ref().map_or(now, |clock| {
                            clock.load(std::sync::atomic::Ordering::Acquire)
                        });
                        if next_due_time > now {
                            let wait_ms = (next_due_time - now).min(5000);
                            sleep(Duration::from_millis(wait_ms)).await;
                            continue;
                        }

                        // Item is due — dispatch FuturesDue signal
                        let timestamp = match i64::try_from(next_due_time) {
                            Ok(millis) => match DateTime::from_timestamp_millis(millis) {
                                Some(dt) => dt,
                                None => {
                                    warn!(
                                        "FutureQueueSource: Due time {next_due_time} is out of range, using current time"
                                    );
                                    chrono::Utc::now()
                                }
                            },
                            Err(e) => {
                                warn!(
                                    "FutureQueueSource: Failed to convert due_time {next_due_time}: {e}, using current time"
                                );
                                chrono::Utc::now()
                            }
                        };
                        (SourceControl::FuturesDue, timestamp)
                    };

                    let event_wrapper = SourceEventWrapper::new(
                        FUTURE_QUEUE_SOURCE_ID.to_string(),
                        SourceEvent::Control(control),
                        timestamp,
                    );
                    let dispatcher_guard = dispatcher_clone.read().await;
                    if let Some(dispatcher) = dispatcher_guard.as_ref() {
                        if let Err(e) = dispatcher.dispatch_change(Arc::new(event_wrapper)).await {
                            debug!("FutureQueueSource failed to dispatch event for query '{query_id}': {e}");
                            #[cfg(feature = "test-support")]
                            if let (Some(control), Some(id)) = (&test_control, request_id) {
                                control.complete(id, Err(crate::error::DrasiError::operation_failed(
                                    "query", &query_id, "dispatch test wake", e.to_string(),
                                )));
                            }
                        }
                    } else {
                        warn!("FutureQueueSource: No dispatcher available for query '{query_id}'");
                        break;
                    }
                    drop(dispatcher_guard);

                    #[cfg(feature = "test-support")]
                    if test_requests.is_some() {
                        continue;
                    }

                    // Post-dispatch throttle: The signaler only peeks, never pops.
                    // Without this sleep, the next peek returns the same due item
                    // causing a tight spin loop until the processor drains it.
                    // 50ms gives the processor time to receive the signal, pop, and commit.
                    sleep(Duration::from_millis(50)).await;
                }

                debug!("FutureQueueSource polling task exited for query '{query_id}'");
            }
            .instrument(span),
        );

        *self.task_handle.write().await = Some(handle);

        Ok(())
    }

    /// Stop the future queue polling task
    pub async fn stop(&self) {
        #[cfg(feature = "test-support")]
        self.test_requests.lock().await.take();
        let mut status = self.status.write().await;
        if *status != FutureQueueSourceStatus::Running {
            return;
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
    }

    /// Get current timestamp in milliseconds since epoch
    pub(crate) fn now() -> u64 {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::interface::{FutureElementRef, IndexError, PushType};
    use drasi_core::models::{ElementReference, ElementTimestamp};

    /// A minimal mock FutureQueue that always returns None from peek_due_time
    struct MockFutureQueue;

    #[async_trait::async_trait]
    impl FutureQueue for MockFutureQueue {
        async fn push(
            &self,
            _push_type: PushType,
            _position_in_query: usize,
            _group_signature: u64,
            _element_ref: &ElementReference,
            _original_time: ElementTimestamp,
            _due_time: ElementTimestamp,
        ) -> Result<bool, IndexError> {
            Ok(true)
        }

        async fn remove(
            &self,
            _position_in_query: usize,
            _group_signature: u64,
        ) -> Result<(), IndexError> {
            Ok(())
        }

        async fn pop(&self) -> Result<Option<FutureElementRef>, IndexError> {
            Ok(None)
        }

        async fn pop_due(
            &self,
            _now: ElementTimestamp,
        ) -> Result<Option<FutureElementRef>, IndexError> {
            Ok(None)
        }

        async fn peek_due_time(&self) -> Result<Option<ElementTimestamp>, IndexError> {
            Ok(None)
        }

        async fn clear(&self) -> Result<(), IndexError> {
            Ok(())
        }
    }

    fn make_source(query_id: &str) -> FutureQueueSource {
        let fq = Arc::new(MockFutureQueue);
        FutureQueueSource::new(fq, query_id.to_string())
    }

    #[test]
    fn new_creates_source_with_correct_query_id() {
        let source = make_source("test-query-1");
        assert_eq!(source.query_id, "test-query-1");
    }

    #[test]
    fn source_id_constant_has_expected_value() {
        assert_eq!(FUTURE_QUEUE_SOURCE_ID, "__future_queue__");
    }

    #[tokio::test]
    async fn initial_status_is_stopped() {
        let source = make_source("status-test");
        let status = source.status.read().await;
        assert_eq!(*status, FutureQueueSourceStatus::Stopped);
    }

    #[tokio::test]
    async fn initial_task_handle_is_none() {
        let source = make_source("handle-test");
        let handle = source.task_handle.read().await;
        assert!(handle.is_none());
    }

    #[tokio::test]
    async fn initial_dispatcher_is_none() {
        let source = make_source("dispatcher-test");
        let dispatcher = source.dispatcher.read().await;
        assert!(dispatcher.is_none());
    }

    #[tokio::test]
    async fn subscribe_returns_receiver() {
        let source = make_source("subscribe-test");
        let result = source.subscribe().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn subscribe_sets_dispatcher() {
        let source = make_source("subscribe-dispatch-test");
        let _ = source.subscribe().await.unwrap();
        let dispatcher = source.dispatcher.read().await;
        assert!(dispatcher.is_some());
    }

    #[tokio::test]
    async fn start_sets_status_to_running() {
        let source = make_source("start-test");
        let _ = source.subscribe().await.unwrap();
        source.start().await.unwrap();
        {
            let status = source.status.read().await;
            assert_eq!(*status, FutureQueueSourceStatus::Running);
        }
        source.stop().await;
    }

    #[tokio::test]
    async fn start_when_already_running_returns_error() {
        let source = make_source("double-start-test");
        let _ = source.subscribe().await.unwrap();
        source.start().await.unwrap();
        let result = source.start().await;
        assert!(result.is_err());
        source.stop().await;
    }

    #[tokio::test]
    async fn stop_sets_status_to_stopped() {
        let source = make_source("stop-test");
        let _ = source.subscribe().await.unwrap();
        source.start().await.unwrap();
        source.stop().await;
        let status = source.status.read().await;
        assert_eq!(*status, FutureQueueSourceStatus::Stopped);
    }

    #[tokio::test]
    async fn stop_clears_dispatcher() {
        let source = make_source("stop-dispatcher-test");
        let _ = source.subscribe().await.unwrap();
        source.start().await.unwrap();
        source.stop().await;
        let dispatcher = source.dispatcher.read().await;
        assert!(dispatcher.is_none());
    }

    #[tokio::test]
    async fn stop_when_already_stopped_is_noop() {
        let source = make_source("stop-noop-test");
        // Stopping a source that was never started should not panic
        source.stop().await;
        let status = source.status.read().await;
        assert_eq!(*status, FutureQueueSourceStatus::Stopped);
    }

    #[tokio::test]
    async fn can_restart_after_stop() {
        let source = make_source("restart-test");
        let _ = source.subscribe().await.unwrap();
        source.start().await.unwrap();
        source.stop().await;

        // Re-subscribe and start again
        let _ = source.subscribe().await.unwrap();
        let result = source.start().await;
        assert!(result.is_ok());
        let status = source.status.read().await;
        assert_eq!(*status, FutureQueueSourceStatus::Running);
        drop(status);
        source.stop().await;
    }
}
