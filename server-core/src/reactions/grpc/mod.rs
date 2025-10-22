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

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, trace, warn};
use rand::Rng;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::RwLock;
use tonic::transport::Channel;

use crate::channels::priority_queue::PriorityQueue;
use crate::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, QueryResult,
};
use crate::config::ReactionConfig;
use crate::reactions::Reaction;
use crate::server_core::DrasiServerCore;
use crate::utils::log_component_start;

// Include generated protobuf code
pub mod proto {
    tonic::include_proto!("drasi.v1");
}

use proto::{
    reaction_service_client::ReactionServiceClient, ProcessResultsRequest,
    QueryResult as ProtoQueryResult, QueryResultItem as ProtoQueryResultItem,
};

/// Connection state for the gRPC client
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Failed,
    Reconnecting,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Disconnected => write!(f, "Disconnected"),
            ConnectionState::Connecting => write!(f, "Connecting"),
            ConnectionState::Connected => write!(f, "Connected"),
            ConnectionState::Failed => write!(f, "Failed"),
            ConnectionState::Reconnecting => write!(f, "Reconnecting"),
        }
    }
}

/// gRPC reaction that sends query results to an external gRPC service
pub struct GrpcReaction {
    config: ReactionConfig,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: ComponentEventSender,
    endpoint: String,
    batch_size: usize,
    batch_flush_timeout_ms: u64,
    timeout_ms: u64,
    max_retries: u32,
    metadata: HashMap<String, String>,
    connection_retry_attempts: u32,
    initial_connection_timeout_ms: u64,
    subscription_tasks: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
    priority_queue: PriorityQueue<QueryResult>,
    processing_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl GrpcReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        // Extract gRPC-specific configuration
        let endpoint = config
            .properties
            .get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("grpc://localhost:50052")
            .to_string();

        let batch_size = config
            .properties
            .get("batch_size")
            .and_then(|v| v.as_u64())
            .unwrap_or(100) as usize;

        let batch_flush_timeout_ms = config
            .properties
            .get("batch_flush_timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(1000); // Default 1 second flush timeout

        let timeout_ms = config
            .properties
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(5000);

        let max_retries = config
            .properties
            .get("max_retries")
            .and_then(|v| v.as_u64())
            .unwrap_or(3) as u32;

        let connection_retry_attempts = config
            .properties
            .get("connection_retry_attempts")
            .and_then(|v| v.as_u64())
            .unwrap_or(5) as u32;

        let initial_connection_timeout_ms = config
            .properties
            .get("initial_connection_timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(10000);

        // Parse metadata if provided
        let mut metadata = HashMap::new();
        if let Some(meta_value) = config.properties.get("metadata") {
            if let Some(meta_obj) = meta_value.as_object() {
                for (key, value) in meta_obj {
                    if let Some(str_value) = value.as_str() {
                        metadata.insert(key.clone(), str_value.to_string());
                    }
                }
            }
        }

        Self {
            config: config.clone(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
            endpoint,
            batch_size,
            batch_flush_timeout_ms,
            timeout_ms,
            max_retries,
            metadata,
            connection_retry_attempts,
            initial_connection_timeout_ms,
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            priority_queue: PriorityQueue::new(config.priority_queue_capacity),
            processing_task: Arc::new(RwLock::new(None)),
        }
    }

    async fn send_batch_with_retry(
        client: &mut ReactionServiceClient<Channel>,
        batch: Vec<ProtoQueryResultItem>,
        query_id: &str,
        metadata: &HashMap<String, String>,
        max_retries: u32,
        endpoint: &str,
        timeout_ms: u64,
    ) -> Result<(bool, Option<ReactionServiceClient<Channel>>)> {
        let mut retries = 0;
        let mut backoff = Duration::from_millis(100);
        let start_time = std::time::Instant::now();
        let max_retry_duration = Duration::from_secs(60); // Max 60 seconds total retry time

        debug!("send_batch_with_retry called - batch_size: {}, query_id: {}, endpoint: {}, max_retries: {}, timeout_ms: {}", 
               batch.len(), query_id, endpoint, max_retries, timeout_ms);

        loop {
            // Check if we've exceeded max retry duration
            if start_time.elapsed() > max_retry_duration {
                warn!(
                    "Max retry duration ({:?}) exceeded after {} attempts",
                    max_retry_duration, retries
                );
                return Ok((true, None)); // Signal connection needs replacement but don't fail
            }
            let attempt_start = std::time::Instant::now();
            debug!(
                "Attempt {}/{} starting at {:?}",
                retries + 1,
                max_retries + 1,
                attempt_start
            );

            let request = tonic::Request::new(ProcessResultsRequest {
                results: Some(ProtoQueryResult {
                    query_id: query_id.to_string(),
                    results: batch.clone(),
                    timestamp: Some(prost_types::Timestamp::from(std::time::SystemTime::now())),
                }),
                metadata: metadata.clone(),
            });

            debug!(
                "Sending request - batch_size: {}, metadata_keys: {:?}, endpoint: {}",
                batch.len(),
                metadata.keys().collect::<Vec<_>>(),
                endpoint
            );

            // Log connection state before sending
            trace!(
                "About to send ProcessResults request - endpoint: {}, query_id: {}, batch_size: {}",
                endpoint,
                query_id,
                batch.len()
            );
            trace!(
                "Connection details - timeout: {}ms, retry: {}/{}",
                timeout_ms,
                retries + 1,
                max_retries + 1
            );

            match client.process_results(request).await {
                Ok(response) => {
                    let elapsed = attempt_start.elapsed();
                    let resp = response.into_inner();
                    debug!(
                        "✓ Response received after {:?} - success: {}, error: '{}'",
                        elapsed, resp.success, resp.error
                    );

                    if resp.success {
                        trace!("✓ Successfully sent batch - size: {}, query_id: {}, time: {:?}, total_time: {:?}", 
                              batch.len(), query_id, elapsed, start_time.elapsed());
                        return Ok((false, None));
                    } else {
                        warn!(
                            "Server returned failure - error: '{}', retries: {}/{}",
                            resp.error, retries, max_retries
                        );
                        if retries >= max_retries {
                            error!("Max retries exceeded - giving up. Error: {}", resp.error);
                            return Err(anyhow::anyhow!(
                                "gRPC call failed after {} retries: {}",
                                max_retries,
                                resp.error
                            ));
                        }
                    }
                }
                Err(e) => {
                    let elapsed = attempt_start.elapsed();
                    let error_str = e.to_string();
                    let error_str_lower = error_str.to_lowercase();

                    // Log full error details
                    error!("Request failed after {:?} - Full error: {}", elapsed, e);
                    debug!(
                        "Error details - code: {:?}, message: {}",
                        e.code(),
                        e.message()
                    );

                    // Categorize the error type for better diagnostics
                    let error_type = if error_str_lower.contains("goaway") {
                        "GoAway"
                    } else if error_str_lower.contains("unavailable") {
                        "Unavailable" // Service temporarily unavailable
                    } else if error_str_lower.contains("deadline") {
                        "DeadlineExceeded" // Request timeout
                    } else if error_str_lower.contains("cancelled") {
                        "Cancelled" // Request cancelled
                    } else if error_str_lower.contains("resource")
                        || error_str_lower.contains("exhausted")
                    {
                        "ResourceExhausted" // Server overloaded
                    } else if error_str_lower.contains("connection")
                        || error_str_lower.contains("transport")
                    {
                        "Connection"
                    } else if error_str_lower.contains("broken pipe")
                        || error_str_lower.contains("connection reset")
                    {
                        "BrokenPipe"
                    } else if error_str_lower.contains("eof")
                        || error_str_lower.contains("channel closed")
                    {
                        "ChannelClosed"
                    } else if error_str_lower.contains("timeout") {
                        "Timeout"
                    } else {
                        "Unknown"
                    };

                    // Determine if this is a transient error that should be retried
                    let _is_transient = matches!(
                        error_type,
                        "GoAway"
                            | "Unavailable"
                            | "DeadlineExceeded"
                            | "ResourceExhausted"
                            | "Connection"
                            | "BrokenPipe"
                            | "ChannelClosed"
                    );

                    let is_connection_error = matches!(
                        error_type,
                        "GoAway" | "Connection" | "BrokenPipe" | "ChannelClosed" | "Unavailable"
                    );

                    warn!(
                        "Error categorized as '{}' - is_connection_error: {}, retry: {}/{}",
                        error_type,
                        is_connection_error,
                        retries + 1,
                        max_retries + 1
                    );

                    if is_connection_error {
                        warn!(
                            "Connection error detected - type: {}, endpoint: {}",
                            error_type, endpoint
                        );

                        // Handle specific error types with appropriate strategies
                        match error_type {
                            "GoAway" => {
                                // Check if this is an immediate GoAway (StreamId(0))
                                if error_str.contains("StreamId(0)") {
                                    error!("Server immediately rejected connection with GoAway(StreamId(0))");
                                    error!("This indicates the server rejected the connection before any streams were created");
                                    error!("Possible causes: incompatible HTTP/2 settings, server not ready, or protocol mismatch");

                                    // For immediate rejection, wait longer before retry
                                    warn!("Waiting 2 seconds before retry due to immediate GoAway");
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                } else {
                                    warn!("GoAway frame received - server closed connection gracefully. This is normal behavior.");
                                }

                                info!("Creating fresh connection after GoAway...");
                                // Always try to create a new client for GoAway - don't reuse existing
                                match Self::create_client_static(endpoint, timeout_ms).await {
                                    Ok(new_client) => {
                                        info!("✓ Successfully created new client after GoAway - will retry request");
                                        return Ok((true, Some(new_client)));
                                    }
                                    Err(create_err) => {
                                        warn!("Failed to create new client after GoAway: {}. Will retry on next batch.", create_err);
                                        return Ok((true, None));
                                    }
                                }
                            }
                            "ResourceExhausted" => {
                                // Server is overloaded, back off more aggressively
                                warn!("Server overloaded (ResourceExhausted) - backing off");
                                tokio::time::sleep(Duration::from_secs(5)).await;
                                if retries >= max_retries {
                                    return Ok((true, None)); // Don't fail, retry on next batch
                                }
                            }
                            "DeadlineExceeded" => {
                                // Request took too long, might need to adjust timeout
                                warn!("Request deadline exceeded - consider increasing timeout");
                                if retries >= max_retries {
                                    return Ok((true, None)); // Don't fail, retry on next batch
                                }
                            }
                            _ => {}
                        }

                        // Don't treat connection errors as fatal - signal for reconnection
                        if retries == 0 {
                            // First attempt failed due to connection - try to create new client immediately
                            debug!("Connection error on first attempt for endpoint {}, signaling for new client", endpoint);
                            match Self::create_client_static(endpoint, timeout_ms).await {
                                Ok(new_client) => {
                                    info!("Successfully created new client for retry");
                                    return Ok((true, Some(new_client)));
                                }
                                Err(create_err) => {
                                    warn!("Failed to create new client: {}. Will retry on next batch.", create_err);
                                    // Return success but signal connection needs replacement
                                    return Ok((true, None));
                                }
                            }
                        } else if retries < max_retries {
                            // Subsequent retries - try once more with new client
                            debug!(
                                "Connection error on retry {}/{}, attempting new client",
                                retries, max_retries
                            );
                            match Self::create_client_static(endpoint, timeout_ms).await {
                                Ok(new_client) => {
                                    debug!("Successfully created new client for retry");
                                    return Ok((true, Some(new_client)));
                                }
                                Err(create_err) => {
                                    warn!("Failed to create new client: {}", create_err);
                                }
                            }
                        } else {
                            // Max retries reached for connection errors - don't fail hard, retry on next batch
                            warn!(
                                "Connection failed after {} retries. Will retry on next batch.",
                                max_retries
                            );
                            return Ok((true, None));
                        }
                    } else {
                        // Non-connection error (e.g., application error)
                        error!("gRPC call failed (type: application): {}", e);
                        if retries >= max_retries {
                            return Err(anyhow::anyhow!(
                                "gRPC call failed after {} retries: {}",
                                max_retries,
                                e
                            ));
                        }
                    }
                }
            }

            retries += 1;

            // Add jitter to backoff to avoid thundering herd
            let jitter = rand::thread_rng().gen_range(0..100);
            let jittered_backoff = backoff + Duration::from_millis(jitter);

            debug!(
                "Retry {}/{} - backing off for {:?} (base: {:?}, jitter: {}ms)",
                retries, max_retries, jittered_backoff, backoff, jitter
            );

            tokio::time::sleep(jittered_backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }
}

#[async_trait]
impl Reaction for GrpcReaction {
    async fn start(&self, server_core: Arc<DrasiServerCore>) -> Result<()> {
        log_component_start("gRPC Reaction", &self.config.id);

        info!(
            "[{}] gRPC reaction starting - sending to endpoint: {}",
            self.config.id, self.endpoint
        );

        *self.status.write().await = ComponentStatus::Starting;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting gRPC reaction".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Get QueryManager from server_core
        let query_manager = server_core.query_manager();

        // Subscribe to all configured queries
        let mut subscription_tasks = self.subscription_tasks.write().await;

        for query_id in &self.config.queries {
            info!("[{}] Subscribing to query '{}'", self.config.id, query_id);

            // Get the query instance
            let query = match query_manager.get_query_instance(query_id).await {
                Ok(q) => q,
                Err(e) => {
                    error!(
                        "[{}] Failed to get query instance '{}': {}",
                        self.config.id, query_id, e
                    );
                    continue;
                }
            };

            // Subscribe to the query
            let subscription_response = match query.subscribe(self.config.id.clone()).await {
                Ok(resp) => resp,
                Err(e) => {
                    error!(
                        "[{}] Failed to subscribe to query '{}': {}",
                        self.config.id, query_id, e
                    );
                    continue;
                }
            };

            let mut broadcast_receiver = subscription_response.broadcast_receiver;
            let priority_queue = self.priority_queue.clone();
            let reaction_id = self.config.id.clone();
            let query_id_clone = query_id.clone();

            // Spawn a forwarder task for this subscription
            let forwarder_task = tokio::spawn(async move {
                info!(
                    "[{}] Forwarder task started for query '{}'",
                    reaction_id, query_id_clone
                );

                loop {
                    match broadcast_receiver.recv().await {
                        Ok(query_result) => {
                            // Enqueue to priority queue for timestamp-ordered processing
                            if !priority_queue.enqueue(query_result).await {
                                warn!(
                                    "[{}] Priority queue full, dropped result from query '{}'",
                                    reaction_id, query_id_clone
                                );
                            }
                        }
                        Err(RecvError::Lagged(count)) => {
                            warn!(
                                "[{}] Forwarder lagged by {} messages for query '{}'",
                                reaction_id, count, query_id_clone
                            );
                            continue;
                        }
                        Err(RecvError::Closed) => {
                            info!(
                                "[{}] Broadcast channel closed for query '{}', forwarder exiting",
                                reaction_id, query_id_clone
                            );
                            break;
                        }
                    }
                }
            });

            subscription_tasks.push(forwarder_task);
        }

        drop(subscription_tasks);

        *self.status.write().await = ComponentStatus::Running;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: Some("gRPC reaction started".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Start processing task that dequeues from priority queue
        let reaction_name = self.config.id.clone();
        let status = Arc::clone(&self.status);
        let endpoint = self.endpoint.clone();
        let batch_size = self.batch_size;
        let batch_flush_timeout_ms = self.batch_flush_timeout_ms;
        let max_retries = self.max_retries;
        let metadata = self.metadata.clone();
        let timeout_ms = self.timeout_ms;
        let connection_retry_attempts = self.connection_retry_attempts;
        let initial_connection_timeout_ms = self.initial_connection_timeout_ms;
        let priority_queue = self.priority_queue.clone();

        let processing_task_handle = tokio::spawn(async move {
            // Lazy connection: do not establish a network connection until data arrives
            info!(
                "gRPC reaction starting for endpoint: {} (lazy connection)",
                endpoint
            );

            let mut client: Option<ReactionServiceClient<Channel>> = None;
            let mut connection_state = ConnectionState::Disconnected;

            // Connection recovery state
            let mut consecutive_failures = 0u32;
            let mut last_connection_attempt = std::time::Instant::now();
            let mut last_successful_send = std::time::Instant::now();
            let base_backoff = Duration::from_millis(500);
            let max_backoff = Duration::from_secs(30);

            // Connection metrics
            let mut total_connection_attempts = 0u32;
            let mut successful_sends = 0u32;
            let mut failed_sends = 0u32;
            let mut goaway_count = 0u32;

            let mut batch = Vec::new();
            let mut last_query_id = String::new();
            let flush_timeout = Duration::from_millis(batch_flush_timeout_ms);

            // Create a timer for batch flushing
            let mut flush_timer = tokio::time::interval(flush_timeout);
            flush_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                // Dequeue from priority queue (blocking)
                let query_result = priority_queue.dequeue().await;
                debug!(
                    "Dequeued query result with {} items for processing",
                    query_result.results.len()
                );

                if !matches!(*status.read().await, ComponentStatus::Running) {
                    info!("[{}] Reaction status changed to non-running, exiting main loop (batch has {} items)",
                          reaction_name, batch.len());
                    break;
                }

                // No proactive idle refresh; rely on error-driven reconnection to honor lazy semantics

                if query_result.results.is_empty() {
                    debug!("[{}] Received empty result set from query", reaction_name);
                    continue;
                }

                let query_id = &query_result.query_id;
                trace!("Processing results for query_id: {}", query_id);

                // If query_id changes and we have a batch, send it
                if !last_query_id.is_empty()
                    && last_query_id != query_result.query_id
                    && !batch.is_empty()
                {
                    // Ensure we have a client (lazy creation)
                    if client.is_none() || !matches!(connection_state, ConnectionState::Connected) {
                        // Check if we should wait before retrying connection
                        let time_since_last_attempt = last_connection_attempt.elapsed();
                        let backoff_duration =
                            base_backoff * 2u32.pow(consecutive_failures.min(10));
                        let backoff_duration = backoff_duration.min(max_backoff);

                        if time_since_last_attempt < backoff_duration
                            && connection_state == ConnectionState::Failed
                        {
                            let wait_time = backoff_duration - time_since_last_attempt;
                            warn!(
                                "State: {} - Waiting {:.2}s before retrying (failures: {})",
                                connection_state,
                                wait_time.as_secs_f64(),
                                consecutive_failures
                            );
                            // Keep the data for next attempt
                            continue;
                        }

                        total_connection_attempts += 1;
                        connection_state = ConnectionState::Connecting;
                        info!(
                            "State transition: {} -> Connecting (attempt {}, total: {})",
                            connection_state,
                            consecutive_failures + 1,
                            total_connection_attempts
                        );
                        last_connection_attempt = std::time::Instant::now();

                        match GrpcReaction::create_client_with_retry(
                            &endpoint,
                            initial_connection_timeout_ms,
                            connection_retry_attempts,
                        )
                        .await
                        {
                            Ok(c) => {
                                connection_state = ConnectionState::Connected;
                                info!("State transition: Connecting -> Connected (after {} failures, total attempts: {})",
                                      consecutive_failures, total_connection_attempts);
                                consecutive_failures = 0; // Reset on success
                                client = Some(c);
                            }
                            Err(e) => {
                                connection_state = ConnectionState::Failed;
                                consecutive_failures += 1;
                                error!("State transition: Connecting -> Failed (attempt {}, total: {}): {}",
                                       consecutive_failures, total_connection_attempts, e);
                                // Don't clear batch - keep it for retry
                                continue;
                            }
                        }
                    }

                    if client.is_some() {
                        // Try to send this batch; if a new client is returned, replace and retry immediately.
                        let mut sent = false;
                        let mut retry_swaps = 0u32;
                        while !sent {
                            if let Some(ref mut c) = client {
                                match Self::send_batch_with_retry(
                                    c,
                                    batch.clone(),
                                    &last_query_id,
                                    &metadata,
                                    max_retries,
                                    &endpoint,
                                    timeout_ms,
                                )
                                .await
                                {
                                    Ok((needs_new_client, new_client)) => {
                                        if needs_new_client {
                                            // Check if this was due to GoAway (heuristic based on timing)
                                            if last_successful_send.elapsed()
                                                > Duration::from_secs(5)
                                            {
                                                goaway_count += 1;
                                            }

                                            if let Some(nc) = new_client {
                                                connection_state = ConnectionState::Connected;
                                                info!(
                                                    "[{}] Replaced client with new connection - State: Connected (GoAway count: {})",
                                                    reaction_name, goaway_count
                                                );
                                                client = Some(nc);
                                                consecutive_failures = 0; // Reset on successful reconnection
                                                retry_swaps += 1;
                                                if retry_swaps > 2 {
                                                    warn!("[{}] Swap limit reached while retrying send; deferring to next batch", reaction_name);
                                                    break;
                                                }
                                                continue; // retry send with new client immediately
                                            } else {
                                                // Connection failed but we'll retry on next batch
                                                connection_state = ConnectionState::Reconnecting;
                                                warn!(
                                                    "[{}] Connection needs replacement - State: Reconnecting",
                                                    reaction_name
                                                );
                                                client = None; // Clear the failed client
                                                consecutive_failures += 1;
                                                last_connection_attempt = std::time::Instant::now();
                                                break; // keep batch for next attempt
                                            }
                                        } else {
                                            // Successful send, reset failure counter
                                            consecutive_failures = 0;
                                            successful_sends += 1;
                                            last_successful_send = std::time::Instant::now();
                                            if successful_sends % 100 == 0 {
                                                info!(
                                                    "[{}] Metrics - State: {}, Successful: {}, Failed: {}, Connections: {}, GoAways: {}",
                                                    reaction_name, connection_state, successful_sends, failed_sends, total_connection_attempts, goaway_count
                                                );
                                            }
                                            sent = true; // clear batch below
                                        }
                                    }
                                    Err(e) => {
                                        failed_sends += 1;
                                        error!(
                                            "[{}] Failed to send batch (total failures: {}): {}",
                                            reaction_name, failed_sends, e
                                        );
                                        break; // keep batch for next attempt
                                    }
                                }
                            } else {
                                break; // no client available; keep batch
                            }
                        }
                        if sent {
                            batch.clear();
                        }
                    }
                }

                last_query_id = query_id.clone();

                // Convert results to protobuf format
                for result in &query_result.results {
                    let proto_item = ProtoQueryResultItem {
                        r#type: result
                            .get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        data: Some(convert_json_to_proto_struct(
                            result.get("data").unwrap_or(result),
                        )),
                        before: result.get("before").map(convert_json_to_proto_struct),
                        after: result.get("after").map(convert_json_to_proto_struct),
                    };

                    batch.push(proto_item);

                    // Send batch if it reaches the configured size
                    if batch.len() >= batch_size {
                        debug!(
                            "[{}] Batch reached size limit ({} >= {}), sending",
                            reaction_name,
                            batch.len(),
                            batch_size
                        );
                        // Ensure we have a client
                        if client.is_none() {
                            // Check if we should wait before retrying connection
                            let time_since_last_attempt = last_connection_attempt.elapsed();
                            let backoff_duration =
                                base_backoff * 2u32.pow(consecutive_failures.min(10));
                            let backoff_duration = backoff_duration.min(max_backoff);

                            if time_since_last_attempt < backoff_duration {
                                let wait_time = backoff_duration - time_since_last_attempt;
                                warn!(
                                    "Waiting {:.2}s before retrying connection (attempt {} failed)",
                                    wait_time.as_secs_f64(),
                                    consecutive_failures
                                );
                                // Keep the data for next attempt
                                continue;
                            }

                            total_connection_attempts += 1;
                            info!("Attempting to create gRPC client for endpoint: {} (failure count: {}, total attempts: {})",
                                  endpoint, consecutive_failures, total_connection_attempts);
                            last_connection_attempt = std::time::Instant::now();

                            match GrpcReaction::create_client_with_retry(
                                &endpoint,
                                initial_connection_timeout_ms,
                                connection_retry_attempts,
                            )
                            .await
                            {
                                Ok(c) => {
                                    info!("Successfully created gRPC client after {} failures (total attempts: {})",
                                          consecutive_failures, total_connection_attempts);
                                    consecutive_failures = 0; // Reset on success
                                    client = Some(c);
                                }
                                Err(e) => {
                                    consecutive_failures += 1;
                                    error!(
                                        "Failed to create client (attempt {}, total: {}): {}",
                                        consecutive_failures, total_connection_attempts, e
                                    );
                                    // Don't clear batch - keep it for retry
                                    continue;
                                }
                            }
                        }

                        if client.is_some() {
                            let mut sent = false;
                            let mut retry_swaps = 0u32;
                            while !sent {
                                if let Some(ref mut c) = client {
                                    match Self::send_batch_with_retry(
                                        c,
                                        batch.clone(),
                                        query_id,
                                        &metadata,
                                        max_retries,
                                        &endpoint,
                                        timeout_ms,
                                    )
                                    .await
                                    {
                                        Ok((needs_new_client, new_client)) => {
                                            if needs_new_client {
                                                if let Some(nc) = new_client {
                                                    info!(
                                                        "[{}] Replaced client with new connection",
                                                        reaction_name
                                                    );
                                                    client = Some(nc);
                                                    consecutive_failures = 0; // Reset on successful reconnection
                                                    retry_swaps += 1;
                                                    if retry_swaps > 2 {
                                                        warn!("[{}] Swap limit reached while retrying send; deferring to next batch", reaction_name);
                                                        break;
                                                    }
                                                    continue; // retry immediately
                                                } else {
                                                    // Connection failed but we'll retry on next batch
                                                    warn!(
                                                        "[{}] Connection needs replacement but no new client available. Will retry on next batch.",
                                                        reaction_name
                                                    );
                                                    client = None; // Clear the failed client
                                                    consecutive_failures += 1;
                                                    last_connection_attempt =
                                                        std::time::Instant::now();
                                                    break; // keep batch
                                                }
                                            } else {
                                                // Successful send, reset failure counter
                                                consecutive_failures = 0;
                                                successful_sends += 1;
                                                if successful_sends % 100 == 0 {
                                                    debug!(
                                                        "[{}] Metrics - Successful sends: {}, Failed: {}, Connection attempts: {}",
                                                        reaction_name, successful_sends, failed_sends, total_connection_attempts
                                                    );
                                                }
                                                sent = true;
                                            }
                                        }
                                        Err(e) => {
                                            failed_sends += 1;
                                            error!("[{}] Failed to send batch (total failures: {}): {}", reaction_name, failed_sends, e);
                                            break; // keep batch
                                        }
                                    }
                                } else {
                                    break; // no client; keep batch
                                }
                            }
                            if sent {
                                batch.clear();
                            }
                        }
                    }
                }
            } // End of loop

            // IMPORTANT: Send any remaining items in the batch when loop exits for ANY reason
            info!(
                "[{}] Main loop exited. Checking for final batch (size: {}, query_id: '{}')",
                reaction_name,
                batch.len(),
                last_query_id
            );

            if !batch.is_empty() {
                if last_query_id.is_empty() {
                    error!("[{}] WARNING: Final batch has {} items but no query_id - this should not happen!", 
                           reaction_name, batch.len());
                    error!(
                        "[{}] Batch will be lost unless query_id is set",
                        reaction_name
                    );
                } else {
                    info!(
                        "[{}] Sending final batch with {} items for query '{}'",
                        reaction_name,
                        batch.len(),
                        last_query_id
                    );

                    // Ensure we have a client
                    if client.is_none() {
                        info!("[{}] Creating gRPC client for final batch", reaction_name);
                        match GrpcReaction::create_client_with_retry(
                            &endpoint,
                            initial_connection_timeout_ms,
                            connection_retry_attempts,
                        )
                        .await
                        {
                            Ok(c) => {
                                info!(
                                    "[{}] Successfully created gRPC client for final batch",
                                    reaction_name
                                );
                                client = Some(c);
                            }
                            Err(e) => {
                                error!(
                                    "[{}] Failed to create client for final batch: {}",
                                    reaction_name, e
                                );
                                error!(
                                    "[{}] WARNING: {} items in final batch will be lost!",
                                    reaction_name,
                                    batch.len()
                                );
                                return;
                            }
                        }
                    }

                    if client.is_some() {
                        let mut sent = false;
                        let mut retry_swaps = 0u32;
                        while !sent {
                            if let Some(ref mut c) = client {
                                match Self::send_batch_with_retry(
                                    c,
                                    batch.clone(),
                                    &last_query_id,
                                    &metadata,
                                    max_retries,
                                    &endpoint,
                                    timeout_ms,
                                )
                                .await
                                {
                                    Ok((needs_new_client, new_client)) => {
                                        if needs_new_client {
                                            if let Some(nc) = new_client {
                                                info!("[{}] Replaced client with new connection for final batch", reaction_name);
                                                client = Some(nc);
                                                retry_swaps += 1;
                                                if retry_swaps > 2 {
                                                    warn!("[{}] Swap limit reached for final batch; aborting send", reaction_name);
                                                    break;
                                                }
                                                continue; // retry immediately
                                            } else {
                                                warn!("[{}] No new client available for final batch; aborting send", reaction_name);
                                                break;
                                            }
                                        } else {
                                            info!(
                                                "[{}] Successfully sent final batch with {} items",
                                                reaction_name,
                                                batch.len()
                                            );
                                            sent = true;
                                        }
                                    }
                                    Err(e) => {
                                        error!(
                                            "[{}] Failed to send final batch: {}",
                                            reaction_name, e
                                        );
                                        break;
                                    }
                                }
                            } else {
                                break; // no client
                            }
                        }
                    }
                } // Close the else block for query_id check
            } else if batch.is_empty() {
                info!(
                    "[{}] No items in final batch, nothing to flush",
                    reaction_name
                );
            }

            info!("[{}] gRPC reaction processing task stopped", reaction_name);
            *status.write().await = ComponentStatus::Stopped;
        });

        // Store the processing task handle
        *self.processing_task.write().await = Some(processing_task_handle);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping gRPC reaction", self.config.id);

        *self.status.write().await = ComponentStatus::Stopping;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Stopping,
            timestamp: chrono::Utc::now(),
            message: Some("Stopping gRPC reaction".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Abort all subscription forwarder tasks
        let mut subscription_tasks = self.subscription_tasks.write().await;
        for task in subscription_tasks.drain(..) {
            task.abort();
        }
        drop(subscription_tasks);
        info!(
            "[{}] Aborted all subscription forwarder tasks",
            self.config.id
        );

        // Abort the processing task
        let mut processing_task = self.processing_task.write().await;
        if let Some(task) = processing_task.take() {
            task.abort();
            info!("[{}] Aborted processing task", self.config.id);
        }
        drop(processing_task);

        // Drain the priority queue
        let drained = self.priority_queue.drain().await;
        if !drained.is_empty() {
            warn!(
                "[{}] Drained {} items from priority queue during stop",
                self.config.id,
                drained.len()
            );
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        *self.status.write().await = ComponentStatus::Stopped;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: Some("gRPC reaction stopped successfully".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &ReactionConfig {
        &self.config
    }
}

impl GrpcReaction {
    async fn create_client_static(
        endpoint: &str,
        timeout_ms: u64,
    ) -> Result<ReactionServiceClient<Channel>> {
        let endpoint = endpoint.replace("grpc://", "http://");
        info!(
            "Creating lazy gRPC channel to {} with timeout: {}ms",
            endpoint, timeout_ms
        );
        let channel = Channel::from_shared(endpoint.clone())?
            .timeout(Duration::from_millis(timeout_ms))
            .connect_lazy();
        info!("Lazy channel created; actual socket connect will occur on first RPC");
        let client = ReactionServiceClient::new(channel);
        info!("ReactionServiceClient created - connection will establish on first RPC call");
        Ok(client)
    }

    async fn create_client_with_retry(
        endpoint: &str,
        timeout_ms: u64,
        max_retries: u32,
    ) -> Result<ReactionServiceClient<Channel>> {
        let mut retries = 0;
        let mut backoff = Duration::from_millis(500);

        loop {
            match Self::create_client_static(endpoint, timeout_ms).await {
                Ok(client) => {
                    info!("Successfully created client for endpoint: {}", endpoint);
                    return Ok(client);
                }
                Err(e) => {
                    if retries >= max_retries {
                        error!(
                            "Failed to create client after {} retries: {}",
                            max_retries, e
                        );
                        return Err(e);
                    }
                    warn!(
                        "Failed to create client (attempt {}/{}): {}",
                        retries + 1,
                        max_retries,
                        e
                    );
                    retries += 1;
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
    }
}

/// Convert JSON value to protobuf Struct
pub fn convert_json_to_proto_struct(value: &serde_json::Value) -> prost_types::Struct {
    use prost_types::Struct;

    match value {
        serde_json::Value::Object(map) => {
            let mut fields = BTreeMap::new();
            for (key, val) in map {
                fields.insert(key.clone(), convert_json_to_proto_value(val));
            }
            Struct { fields }
        }
        _ => {
            // If it's not an object, wrap it in an object with a "value" key
            let mut fields = BTreeMap::new();
            fields.insert("value".to_string(), convert_json_to_proto_value(value));
            Struct { fields }
        }
    }
}

/// Convert JSON value to protobuf Value
fn convert_json_to_proto_value(value: &serde_json::Value) -> prost_types::Value {
    use prost_types::{value::Kind, ListValue, Value};

    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(b) => Kind::BoolValue(*b),
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                Kind::NumberValue(f)
            } else {
                Kind::StringValue(n.to_string())
            }
        }
        serde_json::Value::String(s) => Kind::StringValue(s.clone()),
        serde_json::Value::Array(arr) => {
            let values: Vec<Value> = arr.iter().map(convert_json_to_proto_value).collect();
            Kind::ListValue(ListValue { values })
        }
        serde_json::Value::Object(_) => Kind::StructValue(convert_json_to_proto_struct(value)),
    };

    Value { kind: Some(kind) }
}
