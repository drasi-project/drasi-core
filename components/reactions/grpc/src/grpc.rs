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

use crate::adaptive_batcher::{AdaptiveBatchConfig, AdaptiveBatcher};
use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, trace, warn};
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tonic::transport::Channel;

use drasi_lib::channels::{ComponentEventSender, ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::{QueryProvider, Reaction};

pub use super::config::GrpcReactionConfig;
use super::GrpcReactionBuilder;

// Use modules declared in lib.rs
use crate::connection::{self, create_client, create_client_with_retry, ConnectionState};
use crate::helpers::convert_json_to_proto_struct;
use crate::proto::{
    ProcessResultsRequest, ProtoQueryResult, ProtoQueryResultItem, ReactionServiceClient,
};
use tokio::sync::mpsc;

/// Internal parameters for running the gRPC adaptive reaction
struct RunInternalAdaptiveParams {
    reaction_name: String,
    endpoint: String,
    metadata: HashMap<String, String>,
    #[allow(dead_code)] // Reserved for future use
    timeout_ms: u64,
    max_retries: u32,
    initial_connection_timeout_ms: u64,
    adaptive_config: AdaptiveBatchConfig,
    base: ReactionBase,
}

/// Internal parameters for running gRPC fixed batch reaction
struct RunInternalFixedParams {
    priority_queue: drasi_lib::channels::priority_queue::PriorityQueue<QueryResult>,
    status: Arc<tokio::sync::RwLock<ComponentStatus>>,
    reaction_name: String,
    endpoint: String,
    batch_size: usize,
    batch_flush_timeout_ms: u64,
    max_retries: u32,
    metadata: HashMap<String, String>,
    timeout_ms: u64,
    connection_retry_attempts: u32,
    initial_connection_timeout_ms: u64,
}

/// gRPC reaction that sends query results to an external gRPC service
pub struct GrpcReaction {
    base: ReactionBase,
    config: GrpcReactionConfig,
    adaptive_config: AdaptiveBatchConfig,
}

impl GrpcReaction {
    /// Create a builder for GrpcReaction
    pub fn builder(id: impl Into<String>) -> GrpcReactionBuilder {
        GrpcReactionBuilder::new(id)
    }

    /// Create a new gRPC reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: GrpcReactionConfig) -> Self {
        let id = id.into();
        let utils_adaptive_config = AdaptiveBatchConfig {
            min_batch_size: config.adaptive.adaptive_min_batch_size,
            max_batch_size: config.adaptive.adaptive_max_batch_size,
            throughput_window: Duration::from_millis(
                config.adaptive.adaptive_window_size as u64 * 100,
            ),
            max_wait_time: Duration::from_millis(config.adaptive.adaptive_batch_timeout_ms),
            min_wait_time: Duration::from_millis(100),
            adaptive_enabled: true,
        };
        let params = ReactionBaseParams::new(id, queries);
        Self {
            base: ReactionBase::new(params),
            adaptive_config: utils_adaptive_config,
            config,
        }
    }

    /// Create a new gRPC reaction with custom priority queue capacity
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: GrpcReactionConfig,
        priority_queue_capacity: usize,
    ) -> Self {
        let utils_adaptive_config = AdaptiveBatchConfig {
            min_batch_size: config.adaptive.adaptive_min_batch_size,
            max_batch_size: config.adaptive.adaptive_max_batch_size,
            throughput_window: Duration::from_millis(
                config.adaptive.adaptive_window_size as u64 * 100,
            ),
            max_wait_time: Duration::from_millis(config.adaptive.adaptive_batch_timeout_ms),
            min_wait_time: Duration::from_millis(100),
            adaptive_enabled: true,
        };

        let id = id.into();
        let params = ReactionBaseParams::new(id, queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Self {
            base: ReactionBase::new(params),
            adaptive_config: utils_adaptive_config,
            config,
        }
    }

    /// Create from builder (internal method)
    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: GrpcReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        let utils_adaptive_config = AdaptiveBatchConfig {
            min_batch_size: config.adaptive.adaptive_min_batch_size,
            max_batch_size: config.adaptive.adaptive_max_batch_size,
            throughput_window: Duration::from_millis(
                config.adaptive.adaptive_window_size as u64 * 100,
            ),
            max_wait_time: Duration::from_millis(config.adaptive.adaptive_batch_timeout_ms),
            min_wait_time: Duration::from_millis(100),
            adaptive_enabled: true,
        };
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        Self {
            base: ReactionBase::new(params),
            adaptive_config: utils_adaptive_config,
            config,
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
                    "Max retry duration ({max_retry_duration:?}) exceeded after {retries} attempts"
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
                        "Response received after {:?} - success: {}, error: '{}'",
                        elapsed, resp.success, resp.error
                    );

                    if resp.success {
                        trace!("Successfully sent batch - size: {}, query_id: {}, time: {:?}, total_time: {:?}",
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
                                "gRPC reaction failed: Server returned error after {} retries: {}. \
                                 Check server logs and verify the gRPC endpoint is functioning correctly.",
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
                    error!("Request failed after {elapsed:?} - Full error: {e}");
                    debug!(
                        "Error details - code: {:?}, message: {}",
                        e.code(),
                        e.message()
                    );

                    // Categorize the error type for better diagnostics
                    let error_type = if error_str_lower.contains("goaway") {
                        "GoAway"
                    } else if error_str_lower.contains("unavailable") {
                        "Unavailable"
                    } else if error_str_lower.contains("deadline") {
                        "DeadlineExceeded"
                    } else if error_str_lower.contains("cancelled") {
                        "Cancelled"
                    } else if error_str_lower.contains("resource")
                        || error_str_lower.contains("exhausted")
                    {
                        "ResourceExhausted"
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
                            "Connection error detected - type: {error_type}, endpoint: {endpoint}"
                        );

                        match error_type {
                            "GoAway" => {
                                if error_str.contains("StreamId(0)") {
                                    error!("Server immediately rejected connection with GoAway(StreamId(0))");
                                    warn!("Waiting 2 seconds before retry due to immediate GoAway");
                                    tokio::time::sleep(Duration::from_secs(2)).await;
                                }

                                info!("Creating fresh connection after GoAway...");
                                match create_client(endpoint, timeout_ms).await {
                                    Ok(new_client) => {
                                        info!("Successfully created new client after GoAway - will retry request");
                                        return Ok((true, Some(new_client)));
                                    }
                                    Err(create_err) => {
                                        warn!("Failed to create new client after GoAway: {create_err}. Will retry on next batch.");
                                        return Ok((true, None));
                                    }
                                }
                            }
                            "ResourceExhausted" => {
                                warn!("Server overloaded (ResourceExhausted) - backing off");
                                tokio::time::sleep(Duration::from_secs(5)).await;
                                if retries >= max_retries {
                                    return Ok((true, None));
                                }
                            }
                            "DeadlineExceeded" => {
                                warn!("Request deadline exceeded - consider increasing timeout");
                                if retries >= max_retries {
                                    return Ok((true, None));
                                }
                            }
                            _ => {}
                        }

                        if retries == 0 {
                            debug!("Connection error on first attempt for endpoint {endpoint}, signaling for new client");
                            match create_client(endpoint, timeout_ms).await {
                                Ok(new_client) => {
                                    info!("Successfully created new client for retry");
                                    return Ok((true, Some(new_client)));
                                }
                                Err(create_err) => {
                                    warn!("Failed to create new client: {create_err}. Will retry on next batch.");
                                    return Ok((true, None));
                                }
                            }
                        } else if retries < max_retries {
                            debug!(
                                "Connection error on retry {retries}/{max_retries}, attempting new client"
                            );
                            match create_client(endpoint, timeout_ms).await {
                                Ok(new_client) => {
                                    debug!("Successfully created new client for retry");
                                    return Ok((true, Some(new_client)));
                                }
                                Err(create_err) => {
                                    warn!("Failed to create new client: {create_err}");
                                }
                            }
                        } else {
                            warn!(
                                "Connection failed after {max_retries} retries. Will retry on next batch."
                            );
                            return Ok((true, None));
                        }
                    } else {
                        error!("gRPC call failed (type: application): {e}");
                        if retries >= max_retries {
                            return Err(anyhow::anyhow!(
                                "gRPC reaction failed: Application error after {max_retries} retries: {e}. \
                                 This indicates an error in the receiving application, not a connection issue."
                            ));
                        }
                    }
                }
            }

            retries += 1;

            let jitter = rand::thread_rng().gen_range(0..100);
            let jittered_backoff = backoff + Duration::from_millis(jitter);

            debug!(
                "Retry {retries}/{max_retries} - backing off for {jittered_backoff:?} (base: {backoff:?}, jitter: {jitter}ms)"
            );

            tokio::time::sleep(jittered_backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }

    async fn send_batch(
        client: &mut ReactionServiceClient<Channel>,
        batch: Vec<ProtoQueryResultItem>,
        query_id: &str,
        metadata: &HashMap<String, String>,
    ) -> Result<()> {
        let proto_result = ProtoQueryResult {
            query_id: query_id.to_string(),
            results: batch,
            timestamp: None,
        };

        let request = ProcessResultsRequest {
            results: Some(proto_result),
            metadata: metadata.clone(),
        };

        let mut req = tonic::Request::new(request);

        // Add metadata to request
        for (key, value) in metadata {
            if let Ok(key_str) =
                tonic::metadata::MetadataKey::<tonic::metadata::Ascii>::from_bytes(key.as_bytes())
            {
                if let Ok(value_str) = tonic::metadata::MetadataValue::try_from(value.as_str()) {
                    req.metadata_mut().insert(key_str, value_str);
                }
            }
        }

        let response = client.process_results(req).await?;
        let resp = response.into_inner();

        if !resp.success {
            return Err(anyhow::anyhow!("Batch processing failed: {}", resp.error));
        }

        Ok(())
    }

    async fn create_client(
        endpoint: &str,
        timeout_ms: u64,
    ) -> Result<ReactionServiceClient<Channel>> {
        let channel = Channel::from_shared(endpoint.to_string())?
            .timeout(Duration::from_millis(timeout_ms))
            .connect()
            .await?;

        Ok(ReactionServiceClient::new(channel))
    }

    async fn run_adaptive_internal(
        params: RunInternalAdaptiveParams,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        let reaction_name = params.reaction_name;
        let endpoint = params.endpoint;
        let metadata = params.metadata;
        let max_retries = params.max_retries;
        let initial_connection_timeout_ms = params.initial_connection_timeout_ms;
        let adaptive_config = params.adaptive_config;
        let base = params.base;

        // Create channel for batching with capacity based on batch configuration
        let batch_channel_capacity = adaptive_config.recommended_channel_capacity();
        let (batch_tx, batch_rx) = mpsc::channel(batch_channel_capacity);

        debug!(
            "GrpcAdaptiveReaction using batch channel capacity: {} (max_batch_size: {} × 5)",
            batch_channel_capacity, adaptive_config.max_batch_size
        );

        // Spawn adaptive batcher task
        let batcher_handle = tokio::spawn(async move {
            let mut batcher = AdaptiveBatcher::new(batch_rx, adaptive_config);
            let mut client: Option<ReactionServiceClient<Channel>> = None;
            let mut consecutive_failures = 0u32;
            let mut successful_sends = 0u64;
            let mut failed_sends = 0u64;

            info!("Adaptive gRPC reaction starting for endpoint: {endpoint} (lazy connection)");

            while let Some(batch) = batcher.next_batch().await {
                if batch.is_empty() {
                    continue;
                }

                debug!("Adaptive batch ready with {} items", batch.len());

                // Ensure we have a client
                if client.is_none() {
                    match Self::create_client(&endpoint, initial_connection_timeout_ms).await {
                        Ok(c) => {
                            info!("Successfully created gRPC client for endpoint: {endpoint}");
                            client = Some(c);
                            consecutive_failures = 0;
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            error!("Failed to create client (attempt {consecutive_failures}): {e}");

                            // Exponential backoff
                            let backoff =
                                Duration::from_millis(100 * 2u64.pow(consecutive_failures.min(10)));
                            tokio::time::sleep(backoff).await;
                            continue;
                        }
                    }
                }

                // Send the batch
                {
                    let should_clear_client = if let Some(ref mut c) = client {
                        // Group batch items by query_id
                        let mut batches_by_query: HashMap<String, Vec<ProtoQueryResultItem>> =
                            HashMap::new();
                        for (query_id, items) in batch {
                            batches_by_query.entry(query_id).or_default().extend(items);
                        }

                        let mut needs_clear = false;

                        // Send each query's batch
                        for (query_id, items) in batches_by_query.into_iter() {
                            let mut retries = 0;
                            let mut sent = false;

                            while !sent && retries <= max_retries {
                                match Self::send_batch(c, items.clone(), &query_id, &metadata).await
                                {
                                    Ok(_) => {
                                        sent = true;
                                        successful_sends += 1;
                                        consecutive_failures = 0;

                                        if successful_sends.is_multiple_of(100) {
                                            info!(
                                                "Adaptive metrics - Successful: {successful_sends}, Failed: {failed_sends}"
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        retries += 1;
                                        warn!(
                                            "Failed to send batch (retry {retries}/{max_retries}): {e}"
                                        );

                                        if retries > max_retries {
                                            failed_sends += 1;
                                            error!(
                                                "Failed to send batch after {max_retries} retries"
                                            );
                                            break;
                                        }

                                        // Exponential backoff
                                        let backoff =
                                            Duration::from_millis(100 * 2u64.pow(retries));
                                        tokio::time::sleep(backoff).await;

                                        // Mark that client needs to be cleared
                                        needs_clear = true;
                                    }
                                }
                            }
                        }

                        needs_clear
                    } else {
                        false
                    };

                    // Clear client if needed (outside of borrow scope)
                    if should_clear_client {
                        client = None;
                    }
                }
            }

            info!(
                "Adaptive batcher task completed - Successful: {successful_sends}, Failed: {failed_sends}"
            );
        });

        // Main task: receive query results from priority queue and forward to batcher
        tokio::spawn(async move {
            let mut last_query_id = String::new();
            let mut current_batch = Vec::new();

            loop {
                // Use select to wait for either a result OR shutdown signal
                let query_result = tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                        break;
                    }

                    result = base.priority_queue.dequeue() => result,
                };

                if !matches!(*base.status.read().await, ComponentStatus::Running) {
                    info!("[{reaction_name}] Reaction status changed to non-running, exiting");
                    break;
                }

                if query_result.results.is_empty() {
                    debug!("[{reaction_name}] Received empty result set from query");
                    continue;
                }

                let query_id = query_result.query_id.clone();

                // If query_id changes, send current batch
                if !last_query_id.is_empty()
                    && last_query_id != query_id
                    && !current_batch.is_empty()
                {
                    if batch_tx
                        .send((last_query_id.clone(), current_batch.clone()))
                        .await
                        .is_err()
                    {
                        error!("Failed to send batch to adaptive batcher");
                        break;
                    }
                    current_batch.clear();
                }

                last_query_id = query_id.clone();

                // Convert results to proto format
                for result in &query_result.results {
                    let (result_type, data, before, after) = match result {
                        ResultDiff::Add { data } => ("ADD", data.clone(), None, None),
                        ResultDiff::Delete { data } => ("DELETE", data.clone(), None, None),
                        ResultDiff::Update {
                            data,
                            before,
                            after,
                            ..
                        } => (
                            "UPDATE",
                            data.clone(),
                            Some(before.clone()),
                            Some(after.clone()),
                        ),
                        ResultDiff::Aggregation { before, after } => (
                            "aggregation",
                            serde_json::to_value(result)
                                .expect("ResultDiff serialization should succeed"),
                            before.clone(),
                            Some(after.clone()),
                        ),
                        ResultDiff::Noop => (
                            "noop",
                            serde_json::to_value(result)
                                .expect("ResultDiff serialization should succeed"),
                            None,
                            None,
                        ),
                    };

                    let proto_item = ProtoQueryResultItem {
                        r#type: result_type.to_string(),
                        data: Some(convert_json_to_proto_struct(&data)),
                        before: before.as_ref().map(convert_json_to_proto_struct),
                        after: after.as_ref().map(convert_json_to_proto_struct),
                    };

                    current_batch.push(proto_item);
                }

                // Send immediately if batch is large enough
                if current_batch.len() >= 100 {
                    if batch_tx
                        .send((last_query_id.clone(), current_batch.clone()))
                        .await
                        .is_err()
                    {
                        error!("Failed to send batch to adaptive batcher");
                        break;
                    }
                    current_batch.clear();
                }
            }

            // Send any remaining items
            if !current_batch.is_empty() && !last_query_id.is_empty() {
                let _ = batch_tx.send((last_query_id, current_batch)).await;
            }

            // Close the channel to signal batcher to finish
            drop(batch_tx);

            // Wait for batcher to complete
            let _ = batcher_handle.await;

            info!("[{reaction_name}] Adaptive gRPC reaction completed");
        });
    }

    async fn run_fixed_internal(
        params: RunInternalFixedParams,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        let endpoint = params.endpoint;
        let batch_size = params.batch_size;
        let batch_flush_timeout_ms = params.batch_flush_timeout_ms;
        let max_retries = params.max_retries;
        let metadata = params.metadata;
        let timeout_ms = params.timeout_ms;
        let initial_connection_timeout_ms = params.initial_connection_timeout_ms;
        let reaction_name = params.reaction_name;
        let priority_queue = params.priority_queue;
        let status = params.status;
        let connection_retry_attempts = params.connection_retry_attempts;

        info!("gRPC reaction starting for endpoint: {endpoint} (lazy connection)");

        let mut client: Option<ReactionServiceClient<Channel>> = None;
        let mut connection_state = ConnectionState::Disconnected;

        let mut consecutive_failures = 0u32;
        let mut last_connection_attempt = std::time::Instant::now();
        let mut last_successful_send = std::time::Instant::now();
        let base_backoff = Duration::from_millis(500);
        let max_backoff = Duration::from_secs(30);

        let mut _total_connection_attempts = 0u32;
        let mut _successful_sends = 0u32;
        let mut _failed_sends = 0u32;
        let mut _goaway_count = 0u32;

        let mut batch = Vec::new();
        let mut last_query_id = String::new();
        let flush_timeout = Duration::from_millis(batch_flush_timeout_ms);

        let mut flush_timer = tokio::time::interval(flush_timeout);
        flush_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Use select to wait for either a result OR shutdown signal
            let query_result = tokio::select! {
                biased;

                _ = &mut shutdown_rx => {
                    debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                    break;
                }

                result = priority_queue.dequeue() => result,
            };
            debug!(
                "Dequeued query result with {} items for processing",
                query_result.results.len()
            );

            if !matches!(*status.read().await, ComponentStatus::Running) {
                info!("[{}] Reaction status changed to non-running, exiting main loop (batch has {} items)",
                          reaction_name, batch.len());
                break;
            }

            if query_result.results.is_empty() {
                debug!("[{reaction_name}] Received empty result set from query");
                continue;
            }

            let query_id = &query_result.query_id;
            trace!("Processing results for query_id: {query_id}");

            if !last_query_id.is_empty()
                && last_query_id != query_result.query_id
                && !batch.is_empty()
            {
                if client.is_none() || !matches!(connection_state, ConnectionState::Connected) {
                    let time_since_last_attempt = last_connection_attempt.elapsed();
                    let backoff_duration = base_backoff * 2u32.pow(consecutive_failures.min(10));
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
                        continue;
                    }

                    _total_connection_attempts += 1;
                    let _connecting_state = ConnectionState::Connecting;
                    last_connection_attempt = std::time::Instant::now();

                    match create_client_with_retry(
                        &endpoint,
                        initial_connection_timeout_ms,
                        connection_retry_attempts,
                    )
                    .await
                    {
                        Ok(c) => {
                            connection_state = ConnectionState::Connected;
                            consecutive_failures = 0;
                            client = Some(c);
                        }
                        Err(e) => {
                            connection_state = ConnectionState::Failed;
                            consecutive_failures += 1;
                            error!("State transition: Connecting -> Failed (attempt {consecutive_failures}, total: {_total_connection_attempts}): {e}");
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
                                        if last_successful_send.elapsed() > Duration::from_secs(5) {
                                            _goaway_count += 1;
                                        }

                                        if let Some(nc) = new_client {
                                            connection_state = ConnectionState::Connected;
                                            client = Some(nc);
                                            consecutive_failures = 0;
                                            retry_swaps += 1;
                                            if retry_swaps > 2 {
                                                break;
                                            }
                                            continue;
                                        } else {
                                            connection_state = ConnectionState::Reconnecting;
                                            client = None;
                                            consecutive_failures += 1;
                                            last_connection_attempt = std::time::Instant::now();
                                            break;
                                        }
                                    } else {
                                        consecutive_failures = 0;
                                        _successful_sends += 1;
                                        last_successful_send = std::time::Instant::now();
                                        sent = true;
                                    }
                                }
                                Err(e) => {
                                    _failed_sends += 1;
                                    error!(
                                            "[{reaction_name}] Failed to send batch (total failures: {_failed_sends}): {e}"
                                        );
                                    break;
                                }
                            }
                        } else {
                            break;
                        }
                    }
                    if sent {
                        batch.clear();
                    }
                }
            }

            last_query_id = query_id.clone();

            for result in &query_result.results {
                let (result_type, data, before, after) = match result {
                    ResultDiff::Add { data } => ("ADD", data.clone(), None, None),
                    ResultDiff::Delete { data } => ("DELETE", data.clone(), None, None),
                    ResultDiff::Update {
                        data,
                        before,
                        after,
                        ..
                    } => (
                        "UPDATE",
                        data.clone(),
                        Some(before.clone()),
                        Some(after.clone()),
                    ),
                    ResultDiff::Aggregation { before, after } => (
                        "aggregation",
                        serde_json::to_value(result)
                            .expect("ResultDiff serialization should succeed"),
                        before.clone(),
                        Some(after.clone()),
                    ),
                    ResultDiff::Noop => (
                        "noop",
                        serde_json::to_value(result)
                            .expect("ResultDiff serialization should succeed"),
                        None,
                        None,
                    ),
                };

                let proto_item = ProtoQueryResultItem {
                    r#type: result_type.to_string(),
                    data: Some(convert_json_to_proto_struct(&data)),
                    before: before.as_ref().map(convert_json_to_proto_struct),
                    after: after.as_ref().map(convert_json_to_proto_struct),
                };

                batch.push(proto_item);

                if batch.len() >= batch_size {
                    if client.is_none() {
                        let time_since_last_attempt = last_connection_attempt.elapsed();
                        let backoff_duration =
                            base_backoff * 2u32.pow(consecutive_failures.min(10));
                        let backoff_duration = backoff_duration.min(max_backoff);

                        if time_since_last_attempt < backoff_duration {
                            continue;
                        }

                        _total_connection_attempts += 1;
                        last_connection_attempt = std::time::Instant::now();

                        match create_client_with_retry(
                            &endpoint,
                            initial_connection_timeout_ms,
                            connection_retry_attempts,
                        )
                        .await
                        {
                            Ok(c) => {
                                consecutive_failures = 0;
                                client = Some(c);
                            }
                            Err(e) => {
                                consecutive_failures += 1;
                                error!(
                                        "Failed to create client (attempt {consecutive_failures}, total: {_total_connection_attempts}): {e}"
                                    );
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
                                                client = Some(nc);
                                                consecutive_failures = 0;
                                                retry_swaps += 1;
                                                if retry_swaps > 2 {
                                                    break;
                                                }
                                                continue;
                                            } else {
                                                client = None;
                                                consecutive_failures += 1;
                                                last_connection_attempt = std::time::Instant::now();
                                                break;
                                            }
                                        } else {
                                            consecutive_failures = 0;
                                            _successful_sends += 1;
                                            sent = true;
                                        }
                                    }
                                    Err(e) => {
                                        _failed_sends += 1;
                                        error!("[{reaction_name}] Failed to send batch (total failures: {_failed_sends}): {e}");
                                        break;
                                    }
                                }
                            } else {
                                break;
                            }
                        }
                        if sent {
                            batch.clear();
                        }
                    }
                }
            }
        }

        // Send any remaining items in the batch
        if !batch.is_empty() && !last_query_id.is_empty() {
            info!(
                "[{}] Sending final batch with {} items for query '{}'",
                reaction_name,
                batch.len(),
                last_query_id
            );

            if client.is_none() {
                match create_client_with_retry(
                    &endpoint,
                    initial_connection_timeout_ms,
                    connection_retry_attempts,
                )
                .await
                {
                    Ok(c) => {
                        client = Some(c);
                    }
                    Err(e) => {
                        error!("[{reaction_name}] Failed to create client for final batch: {e}");
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
                                        client = Some(nc);
                                        retry_swaps += 1;
                                        if retry_swaps > 2 {
                                            break;
                                        }
                                        continue;
                                    } else {
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
                                error!("[{reaction_name}] Failed to send final batch: {e}");
                                break;
                            }
                        }
                    } else {
                        break;
                    }
                }
            }
        }

        info!("[{reaction_name}] gRPC reaction processing task stopped");
        *status.write().await = ComponentStatus::Stopped;
    }
}

#[async_trait]
impl Reaction for GrpcReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "grpc"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "endpoint".to_string(),
            serde_json::Value::String(self.config.endpoint.clone()),
        );
        props.insert(
            "timeout_ms".to_string(),
            serde_json::Value::Number(self.config.timeout_ms.into()),
        );
        if !self.config.adaptive_enable {
            props.insert(
                "batch_size".to_string(),
                serde_json::Value::Number(self.config.batch_size.into()),
            );
        } else {
            props.insert(
                "max_retries".to_string(),
                serde_json::Value::Number(self.config.max_retries.into()),
            );
        }
        props
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("gRPC Reaction", &self.base.id);

        info!(
            "[{}] gRPC reaction starting - sending to endpoint: {}, adaptive_enable: {}",
            self.base.id, self.config.endpoint, self.config.adaptive_enable
        );

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting gRPC reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        // QueryProvider is available from initialize() context
        self.base.subscribe_to_queries().await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("gRPC reaction started".to_string()),
            )
            .await?;

        // Create shutdown channel for graceful termination
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // Start processing task that dequeues from priority queue
        let reaction_name = self.base.id.clone();
        let status = self.base.status.clone();
        let endpoint = self.config.endpoint.clone();
        let batch_size = self.config.batch_size;
        let batch_flush_timeout_ms = self.config.batch_flush_timeout_ms;
        let max_retries = self.config.max_retries;
        let metadata = self.config.metadata.clone();
        let timeout_ms = self.config.timeout_ms;
        let connection_retry_attempts = self.config.connection_retry_attempts;
        let initial_connection_timeout_ms = self.config.initial_connection_timeout_ms;
        let priority_queue = self.base.priority_queue.clone();
        let adaptive_config = self.adaptive_config.clone();
        let base = self.base.clone_shared();
        let adaptive_enable = self.config.adaptive_enable;

        let processing_task_handle = tokio::spawn(async move {
            if !adaptive_enable {
                let params = RunInternalFixedParams {
                    reaction_name,
                    endpoint,
                    metadata,
                    timeout_ms,
                    batch_size,
                    batch_flush_timeout_ms,
                    max_retries,
                    connection_retry_attempts,
                    initial_connection_timeout_ms,
                    priority_queue,
                    status,
                };
                Self::run_fixed_internal(params, shutdown_rx).await
            } else {
                let params = RunInternalAdaptiveParams {
                    reaction_name,
                    endpoint,
                    metadata,
                    timeout_ms,
                    max_retries,
                    initial_connection_timeout_ms,
                    adaptive_config,
                    base,
                };
                Self::run_adaptive_internal(params, shutdown_rx).await
            }
        });

        self.base.set_processing_task(processing_task_handle).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await?;

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("gRPC reaction stopped successfully".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
}
