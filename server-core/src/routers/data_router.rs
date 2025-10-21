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

use log::{debug, error, info, warn};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use std::time::{Duration, Instant};

use crate::channels::{
    ControlSignal, ControlSignalReceiver, ControlSignalWrapper,
    QueryResult, QueryResultSender,
    SourceEvent, SourceEventReceiver, SourceEventSender, SourceEventWrapper
};
use serde_json::json;

/// Bootstrap phase for a query-source pair
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BootstrapPhase {
    NotStarted,
    InProgress,
    Completed,
}

/// Key for identifying a query-source pair
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct QuerySourceKey {
    query_id: String,
    source_id: String,
}

/// Routes data changes from sources to subscribed queries
#[derive(Clone)]
pub struct DataRouter {
    /// Map of query id to its event sender channel
    query_event_senders: Arc<RwLock<HashMap<String, SourceEventSender>>>,
    /// Map of source id to list of query ids that subscribe to it
    source_subscriptions: Arc<RwLock<HashMap<String, Vec<String>>>>,
    /// Bootstrap state per query-source pair
    bootstrap_state: Arc<RwLock<HashMap<QuerySourceKey, BootstrapPhase>>>,
    /// Event buffer for queries in bootstrap phase
    event_buffer: Arc<RwLock<HashMap<QuerySourceKey, VecDeque<SourceEventWrapper>>>>,
    /// Maximum buffer size per query-source pair
    max_buffer_size: usize,
    /// Bootstrap timeout duration
    bootstrap_timeout: Duration,
    /// Track bootstrap start times for timeout detection
    bootstrap_start_times: Arc<RwLock<HashMap<QuerySourceKey, Instant>>>,
    /// Query result sender for emitting control signals to reactions
    query_result_tx: Option<QueryResultSender>,
}

impl Default for DataRouter {
    fn default() -> Self {
        Self {
            query_event_senders: Arc::new(RwLock::new(HashMap::new())),
            source_subscriptions: Arc::new(RwLock::new(HashMap::new())),
            bootstrap_state: Arc::new(RwLock::new(HashMap::new())),
            event_buffer: Arc::new(RwLock::new(HashMap::new())),
            max_buffer_size: 10000, // Default buffer size
            bootstrap_timeout: Duration::from_secs(300), // 5 minute default timeout
            bootstrap_start_times: Arc::new(RwLock::new(HashMap::new())),
            query_result_tx: None,
        }
    }
}

impl DataRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the query result sender for emitting control signals
    pub fn set_query_result_tx(&mut self, tx: QueryResultSender) {
        self.query_result_tx = Some(tx);
    }

    /// Emit a control signal as a QueryResult to reactions
    async fn emit_control_signal_as_result(&self, query_id: &str, signal_type: &str) {
        if let Some(ref tx) = self.query_result_tx {
            let mut metadata = HashMap::new();
            metadata.insert("control_signal".to_string(), json!(signal_type));

            // Create a synthetic QueryResult representing the control signal
            let control_result = QueryResult::new(
                query_id.to_string(),
                chrono::Utc::now(),
                vec![], // No data results for control signals
                metadata,
            );

            if let Err(e) = tx.send(control_result).await {
                warn!("Failed to emit control signal '{}' as QueryResult for query '{}': {}",
                    signal_type, query_id, e);
            } else {
                info!("Emitted control signal '{}' as QueryResult for query '{}'",
                    signal_type, query_id);
            }
        } else {
            debug!("No query_result_tx configured, control signal '{}' not emitted to reactions", signal_type);
        }
    }

    /// Add a query subscription to specific sources
    ///
    /// If a subscription for this query already exists, this method will return an error
    /// to prevent accidentally closing the existing subscription channel.
    ///
    /// # Arguments
    /// * `query_id` - The ID of the query subscribing
    /// * `source_ids` - The source IDs to subscribe to
    /// * `enable_bootstrap` - Whether to enable bootstrap for this query
    pub async fn add_query_subscription(
        &self,
        query_id: String,
        source_ids: Vec<String>,
        enable_bootstrap: bool,
    ) -> Result<SourceEventReceiver, String> {
        let start_time = std::time::Instant::now();

        // Log with backtrace information for debugging
        let backtrace = std::backtrace::Backtrace::force_capture();
        let backtrace_str = backtrace.to_string();
        let caller_info: Vec<&str> = backtrace_str
            .lines()
            .filter(|line| line.contains("drasi_server") && !line.contains("data_router"))
            .take(3)
            .collect();

        info!(
            "[SUBSCRIPTION-TRACE] add_query_subscription called for query '{}' to sources: {:?}. Caller: {:?}",
            query_id, source_ids, caller_info
        );

        // Check if query already has a subscription
        let existing_sender = self
            .query_event_senders
            .read()
            .await
            .contains_key(&query_id);
        if existing_sender {
            let error_msg = format!(
                "[DUPLICATE-SUBSCRIPTION] Query '{}' already has an active subscription. This is the SECOND call - first call succeeded. Caller trace: {:?}",
                query_id, caller_info
            );
            error!("{}", error_msg);
            return Err(error_msg);
        }

        let (tx, rx) = mpsc::channel(1000);

        // Store the sender for this query (overwrites existing if any)
        self.query_event_senders
            .write()
            .await
            .insert(query_id.clone(), tx);
        info!("Channel created and sender stored for query '{}'", query_id);

        // CRITICAL: Set bootstrap phase BEFORE adding subscriptions
        // This prevents race condition where events arrive before phase is set
        if enable_bootstrap {
            info!("[BOOTSTRAP] Query '{}' enabling bootstrap for {} sources", query_id, source_ids.len());
            for source_id in &source_ids {
                self.set_bootstrap_phase(query_id.clone(), source_id.clone(), BootstrapPhase::InProgress).await;
                info!("[BOOTSTRAP] Set state to InProgress for query '{}' source '{}'", query_id, source_id);
            }
        } else {
            info!("[BOOTSTRAP] Query '{}' bootstrap disabled, events will flow immediately", query_id);
        }

        // Update source subscriptions AFTER setting bootstrap phase
        // This ensures any events that arrive will see the correct phase
        let mut subscriptions = self.source_subscriptions.write().await;
        for source_id in &source_ids {
            let query_list = subscriptions
                .entry(source_id.clone())
                .or_insert_with(Vec::new);

            // Only add if not already subscribed
            if !query_list.contains(&query_id) {
                query_list.push(query_id.clone());
                info!(
                    "Query '{}' successfully subscribed to source '{}'",
                    query_id, source_id
                );
            } else {
                debug!(
                    "Query '{}' already subscribed to source '{}'",
                    query_id, source_id
                );
            }
        }

        let elapsed = start_time.elapsed();
        info!(
            "Completed subscription for query '{}' to {} sources in {:?}",
            query_id,
            source_ids.len(),
            elapsed
        );

        Ok(rx)
    }

    /// Remove a query subscription
    pub async fn remove_query_subscription(&self, query_id: &str) {
        // Remove the sender
        self.query_event_senders.write().await.remove(query_id);

        // Remove from all source subscriptions
        let mut subscriptions = self.source_subscriptions.write().await;
        for queries in subscriptions.values_mut() {
            queries.retain(|id| id != query_id);
        }

        // Clean up bootstrap state and buffers for this query
        let mut bootstrap_state = self.bootstrap_state.write().await;
        let mut event_buffer = self.event_buffer.write().await;
        let mut start_times = self.bootstrap_start_times.write().await;

        // Remove all entries for this query
        bootstrap_state.retain(|key, _| key.query_id != query_id);
        event_buffer.retain(|key, _| key.query_id != query_id);
        start_times.retain(|key, _| key.query_id != query_id);

        info!("Removed subscription for query '{}'", query_id);
    }

    /// Set bootstrap phase for a query-source pair
    async fn set_bootstrap_phase(&self, query_id: String, source_id: String, phase: BootstrapPhase) {
        let key = QuerySourceKey { query_id: query_id.clone(), source_id: source_id.clone() };
        let mut states = self.bootstrap_state.write().await;

        info!("Setting bootstrap phase for query '{}' source '{}' to {:?}",
            query_id, source_id, phase);

        states.insert(key.clone(), phase);

        // Track start time when entering InProgress
        if phase == BootstrapPhase::InProgress {
            let mut start_times = self.bootstrap_start_times.write().await;
            start_times.insert(key, Instant::now());
        }
        // Clean up start time when completed
        else if phase == BootstrapPhase::Completed {
            let mut start_times = self.bootstrap_start_times.write().await;
            start_times.remove(&key);
        }
    }

    /// Get bootstrap phase for a query-source pair
    async fn get_bootstrap_phase(&self, query_id: &str, source_id: &str) -> BootstrapPhase {
        let key = QuerySourceKey {
            query_id: query_id.to_string(),
            source_id: source_id.to_string()
        };
        let states = self.bootstrap_state.read().await;
        states.get(&key).copied().unwrap_or(BootstrapPhase::NotStarted)
    }

    /// Buffer an event for a query-source pair during bootstrap
    async fn buffer_event(&self, query_id: String, source_id: String, event: SourceEventWrapper) -> Result<(), String> {
        let key = QuerySourceKey { query_id: query_id.clone(), source_id: source_id.clone() };
        let mut buffers = self.event_buffer.write().await;

        let buffer = buffers.entry(key).or_insert_with(VecDeque::new);

        // Check buffer size limit
        if buffer.len() >= self.max_buffer_size {
            warn!("Event buffer for query '{}' source '{}' is full ({}). Dropping oldest event.",
                query_id, source_id, self.max_buffer_size);
            buffer.pop_front(); // Drop oldest event
        }

        buffer.push_back(event);
        debug!("Buffered event for query '{}' source '{}'. Buffer size: {}",
            query_id, source_id, buffer.len());

        Ok(())
    }

    /// Release buffered events for a query-source pair after bootstrap completes
    async fn release_buffered_events(&self, query_id: String, source_id: String) {
        let key = QuerySourceKey { query_id: query_id.clone(), source_id: source_id.clone() };

        // Remove the buffer to take ownership
        let mut buffers = self.event_buffer.write().await;
        if let Some(buffer) = buffers.remove(&key) {
            let count = buffer.len();
            info!("Releasing {} buffered events for query '{}' source '{}'",
                count, query_id, source_id);

            // Get the sender for this query
            let senders = self.query_event_senders.read().await;
            if let Some(sender) = senders.get(&query_id) {
                // Send all buffered events
                for event in buffer {
                    match sender.send(event).await {
                        Ok(_) => {
                            debug!("Released buffered event to query '{}'", query_id);
                        }
                        Err(e) => {
                            error!("Failed to release buffered event to query '{}': {}", query_id, e);
                            break; // Stop if channel is closed
                        }
                    }
                }
                info!("Successfully released {} buffered events for query '{}' source '{}'",
                    count, query_id, source_id);
            } else {
                warn!("No sender found for query '{}' when releasing buffered events", query_id);
            }
        } else {
            debug!("No buffered events to release for query '{}' source '{}'", query_id, source_id);
        }
    }

    /// Handle bootstrap end marker - ensures all bootstrap data has been received
    /// before transitioning to Completed state
    async fn handle_bootstrap_end_marker(&self, query_id: String, source_id: String) {
        info!("[BOOTSTRAP] Processing BootstrapEnd marker for query '{}' source '{}'",
            query_id, source_id);

        // Check current bootstrap phase
        let phase = self.get_bootstrap_phase(&query_id, &source_id).await;

        if phase != BootstrapPhase::InProgress {
            warn!("[BOOTSTRAP] Received BootstrapEnd marker for query '{}' source '{}' but phase is {:?}, ignoring",
                query_id, source_id, phase);
            return;
        }

        info!("[BOOTSTRAP] Completing bootstrap for query '{}' source '{}' - all data received",
            query_id, source_id);

        // Transition to Completed state now that all data has arrived
        self.set_bootstrap_phase(query_id.clone(), source_id.clone(), BootstrapPhase::Completed).await;

        // Release buffered change events
        self.release_buffered_events(query_id.clone(), source_id.clone()).await;

        // Forward the BootstrapEnd marker to the query so it can emit the bootstrapCompleted signal
        // after it has finished processing all the bootstrap data
        let marker = SourceEventWrapper::new(
            source_id.clone(),
            SourceEvent::BootstrapEnd {
                query_id: query_id.clone(),
            },
            chrono::Utc::now(),
        );

        let senders = self.query_event_senders.read().await;
        if let Some(sender) = senders.get(&query_id) {
            if let Err(e) = sender.send(marker).await {
                error!("Failed to send BootstrapEnd marker to query '{}': {}", query_id, e);
            } else {
                info!("[BOOTSTRAP] Forwarded BootstrapEnd marker to query '{}' for final signal emission", query_id);
            }
        } else {
            warn!("No sender found for query '{}' when forwarding BootstrapEnd marker", query_id);
        }

        info!("[BOOTSTRAP] Bootstrap data delivery completed for query '{}' source '{}'", query_id, source_id);
    }

    /// Check if bootstrap has timed out for any query-source pairs
    async fn check_bootstrap_timeouts(&self) {
        let now = Instant::now();

        // Collect timed-out pairs first to avoid holding lock while updating
        let timed_out_pairs: Vec<QuerySourceKey> = {
            let start_times = self.bootstrap_start_times.read().await;
            start_times
                .iter()
                .filter_map(|(key, start_time)| {
                    if now.duration_since(*start_time) > self.bootstrap_timeout {
                        Some(key.clone())
                    } else {
                        None
                    }
                })
                .collect()
        };

        // Process timed-out pairs
        for key in timed_out_pairs {
            warn!("Bootstrap timeout for query '{}' source '{}' after {:?}",
                key.query_id, key.source_id, self.bootstrap_timeout);

            // Force transition to Completed state
            self.set_bootstrap_phase(key.query_id.clone(), key.source_id.clone(), BootstrapPhase::Completed).await;
            self.release_buffered_events(key.query_id, key.source_id).await;
        }
    }

    /// Route a source event with bootstrap checking
    async fn route_source_event(&self, source_event_wrapper: SourceEventWrapper) {
        let source_id = &source_event_wrapper.source_id;

        // Log event type for visibility
        match &source_event_wrapper.event {
            SourceEvent::Change(_) => {
                debug!("Data router received Change event from source '{}'", source_id);
            }
            SourceEvent::Control(_) => {
                debug!("Data router received Control event from source '{}'", source_id);
            }
            SourceEvent::BootstrapStart { query_id } => {
                debug!("Data router received BootstrapStart marker from source '{}' for query '{}'",
                    source_id, query_id);
                // These markers are now replaced by control signals
                return;
            }
            SourceEvent::BootstrapEnd { query_id } => {
                info!("[BOOTSTRAP] Data router received BootstrapEnd marker from source '{}' for query '{}'",
                    source_id, query_id);
                // Process the bootstrap end marker - this ensures all bootstrap data has arrived
                self.handle_bootstrap_end_marker(query_id.clone(), source_id.clone()).await;
                return;
            }
        }

        // Get list of queries subscribed to this source
        let subscriptions = self.source_subscriptions.read().await;
        if let Some(query_ids) = subscriptions.get(source_id) {
            debug!("[DATA-FLOW] Source '{}' has {} subscribed queries",
                source_id, query_ids.len());

            for query_id in query_ids {
                // Check bootstrap phase for this query-source pair
                let phase = self.get_bootstrap_phase(query_id, source_id).await;

                match phase {
                    BootstrapPhase::NotStarted | BootstrapPhase::Completed => {
                        // Normal operation - forward the event
                        let senders = self.query_event_senders.read().await;
                        if let Some(sender) = senders.get(query_id) {
                            debug!("[DATA-FLOW] Forwarding event from source '{}' to query '{}'",
                                source_id, query_id);
                            match sender.send(source_event_wrapper.clone()).await {
                                Ok(_) => {
                                    debug!("Routed event from source '{}' to query '{}'",
                                        source_id, query_id);
                                }
                                Err(e) => {
                                    error!("Failed to send event to query '{}': {}", query_id, e);
                                }
                            }
                        }
                    }
                    BootstrapPhase::InProgress => {
                        // Bootstrap in progress - buffer the event
                        match &source_event_wrapper.event {
                            SourceEvent::Change(_) => {
                                info!("[BOOTSTRAP] Buffering change event for query '{}' source '{}' during bootstrap",
                                    query_id, source_id);
                                if let Err(e) = self.buffer_event(
                                    query_id.clone(),
                                    source_id.clone(),
                                    source_event_wrapper.clone()
                                ).await {
                                    error!("Failed to buffer event: {}", e);
                                }
                            }
                            SourceEvent::Control(_) => {
                                // Control events can be forwarded during bootstrap
                                let senders = self.query_event_senders.read().await;
                                if let Some(sender) = senders.get(query_id) {
                                    debug!("[BOOTSTRAP] Forwarding control event to query '{}' during bootstrap",
                                        query_id);
                                    let _ = sender.send(source_event_wrapper.clone()).await;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        } else {
            debug!("No queries subscribed to source '{}'", source_id);
        }
    }

    /// Handle a control signal
    async fn handle_control_signal(&self, signal_wrapper: ControlSignalWrapper) {
        match signal_wrapper.signal {
            ControlSignal::BootstrapStarted { query_id, source_id } => {
                info!("Received BootstrapStarted signal for query '{}' source '{}'",
                    query_id, source_id);
                self.set_bootstrap_phase(query_id.clone(), source_id, BootstrapPhase::InProgress).await;

                // Emit bootstrapStarted control signal as QueryResult
                self.emit_control_signal_as_result(&query_id, "bootstrapStarted").await;
            }
            ControlSignal::BootstrapCompleted { query_id, source_id } => {
                // This control signal is now deprecated in favor of BootstrapEnd marker
                // which flows through the same channel as bootstrap data
                debug!("Received deprecated BootstrapCompleted signal for query '{}' source '{}' - ignoring (using BootstrapEnd marker instead)",
                    query_id, source_id);
                // The actual completion is now handled by BootstrapEnd marker
                // which ensures all bootstrap data has been delivered before completion
            }
            ControlSignal::Running { query_id } => {
                debug!("Query '{}' is now running", query_id);
            }
            ControlSignal::Stopped { query_id } => {
                debug!("Query '{}' has stopped", query_id);
            }
            ControlSignal::Deleted { query_id } => {
                debug!("Query '{}' has been deleted", query_id);
                self.remove_query_subscription(&query_id).await;
            }
        }
    }

    /// Start the router that receives source events and forwards them to subscribed queries
    pub async fn start(&self, mut source_event_rx: SourceEventReceiver, mut control_signal_rx: ControlSignalReceiver) {
        info!("Starting data router with control signal support");

        // Spawn a periodic task to check for bootstrap timeouts
        let timeout_checker = self.clone();
        let timeout_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            loop {
                interval.tick().await;
                timeout_checker.check_bootstrap_timeouts().await;
            }
        });

        loop {
            tokio::select! {
                Some(source_event_wrapper) = source_event_rx.recv() => {
                    self.route_source_event(source_event_wrapper).await;
                }
                Some(control_signal_wrapper) = control_signal_rx.recv() => {
                    self.handle_control_signal(control_signal_wrapper).await;
                }
                else => {
                    // Both channels are closed, exit the loop
                    break;
                }
            }
        }

        // Cancel the timeout checker task
        timeout_handle.abort();

        info!("Data router stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_new_router() {
        let router = DataRouter::new();

        // Verify initial state
        let senders = router.query_event_senders.read().await;
        let subscriptions = router.source_subscriptions.read().await;

        assert!(senders.is_empty());
        assert!(subscriptions.is_empty());
    }

    #[tokio::test]
    async fn test_add_single_subscription() {
        let router = DataRouter::new();

        let rx = router
            .add_query_subscription("query1".to_string(), vec!["source1".to_string()], false)
            .await
            .expect("Should create subscription");

        // Verify query sender is stored
        let senders = router.query_event_senders.read().await;
        assert!(senders.contains_key("query1"));

        // Verify source subscription is recorded
        let subscriptions = router.source_subscriptions.read().await;
        assert_eq!(
            subscriptions.get("source1"),
            Some(&vec!["query1".to_string()])
        );

        // Verify receiver is functional
        assert!(!rx.is_closed());
    }

    #[tokio::test]
    async fn test_multiple_queries_same_source() {
        let router = DataRouter::new();

        let _rx1 = router
            .add_query_subscription("query1".to_string(), vec!["source1".to_string()])
            .await
            .expect("Should create subscription for query1");

        let _rx2 = router
            .add_query_subscription("query2".to_string(), vec!["source1".to_string()])
            .await
            .expect("Should create subscription for query2");

        let subscriptions = router.source_subscriptions.read().await;
        let source1_queries = subscriptions.get("source1").unwrap();

        assert_eq!(source1_queries.len(), 2);
        assert!(source1_queries.contains(&"query1".to_string()));
        assert!(source1_queries.contains(&"query2".to_string()));
    }

    #[tokio::test]
    async fn test_remove_subscription() {
        let router = DataRouter::new();

        let _rx = router
            .add_query_subscription("query1".to_string(), vec!["source1".to_string()])
            .await
            .expect("Should create subscription");

        // Verify subscription exists
        assert!(router
            .query_event_senders
            .read()
            .await
            .contains_key("query1"));

        // Remove subscription
        router.remove_query_subscription("query1").await;

        // Verify removal
        let senders = router.query_event_senders.read().await;
        assert!(!senders.contains_key("query1"));
    }

    #[tokio::test]
    async fn test_duplicate_subscription_fails() {
        let router = DataRouter::new();

        // First subscription should succeed
        let _rx1 = router
            .add_query_subscription("query1".to_string(), vec!["source1".to_string()])
            .await
            .expect("First subscription should succeed");

        // Second subscription for same query should fail
        let result = router
            .add_query_subscription("query1".to_string(), vec!["source1".to_string()])
            .await;

        assert!(result.is_err(), "Duplicate subscription should fail");
        assert!(result
            .unwrap_err()
            .contains("already has an active subscription"));
    }
}
