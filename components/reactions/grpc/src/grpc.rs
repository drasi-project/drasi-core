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
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tonic::transport::Channel;

use drasi_lib::channels::{ComponentEventSender, ComponentStatus};
use drasi_lib::plugin_core::{QuerySubscriber, Reaction};
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::managers::log_component_start;

pub use super::config::GrpcReactionConfig;
use super::GrpcReactionBuilder;

// Use modules declared in lib.rs
use crate::connection::{create_client, create_client_with_retry, ConnectionState};
use crate::helpers::convert_json_to_proto_struct;
use crate::proto::{
    ProcessResultsRequest, ProtoQueryResult, ProtoQueryResultItem, ReactionServiceClient,
};

/// gRPC reaction that sends query results to an external gRPC service
pub struct GrpcReaction {
    base: ReactionBase,
    config: GrpcReactionConfig,
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
        let params = ReactionBaseParams::new(id, queries);
        Self {
            base: ReactionBase::new(params),
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
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Self {
            base: ReactionBase::new(params),
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
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        Self {
            base: ReactionBase::new(params),
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
                            "Connection error detected - type: {}, endpoint: {}",
                            error_type, endpoint
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
                                        warn!("Failed to create new client after GoAway: {}. Will retry on next batch.", create_err);
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
                            debug!("Connection error on first attempt for endpoint {}, signaling for new client", endpoint);
                            match create_client(endpoint, timeout_ms).await {
                                Ok(new_client) => {
                                    info!("Successfully created new client for retry");
                                    return Ok((true, Some(new_client)));
                                }
                                Err(create_err) => {
                                    warn!("Failed to create new client: {}. Will retry on next batch.", create_err);
                                    return Ok((true, None));
                                }
                            }
                        } else if retries < max_retries {
                            debug!(
                                "Connection error on retry {}/{}, attempting new client",
                                retries, max_retries
                            );
                            match create_client(endpoint, timeout_ms).await {
                                Ok(new_client) => {
                                    debug!("Successfully created new client for retry");
                                    return Ok((true, Some(new_client)));
                                }
                                Err(create_err) => {
                                    warn!("Failed to create new client: {}", create_err);
                                }
                            }
                        } else {
                            warn!(
                                "Connection failed after {} retries. Will retry on next batch.",
                                max_retries
                            );
                            return Ok((true, None));
                        }
                    } else {
                        error!("gRPC call failed (type: application): {}", e);
                        if retries >= max_retries {
                            return Err(anyhow::anyhow!(
                                "gRPC reaction failed: Application error after {} retries: {}. \
                                 This indicates an error in the receiving application, not a connection issue.",
                                max_retries,
                                e
                            ));
                        }
                    }
                }
            }

            retries += 1;

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
            "batch_size".to_string(),
            serde_json::Value::Number(self.config.batch_size.into()),
        );
        props.insert(
            "timeout_ms".to_string(),
            serde_json::Value::Number(self.config.timeout_ms.into()),
        );
        props
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
        log_component_start("gRPC Reaction", &self.base.id);

        info!(
            "[{}] gRPC reaction starting - sending to endpoint: {}",
            self.base.id, self.config.endpoint
        );

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting gRPC reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        // QuerySubscriber was injected via inject_query_subscriber() when reaction was added
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

        let processing_task_handle = tokio::spawn(async move {
            info!(
                "gRPC reaction starting for endpoint: {} (lazy connection)",
                endpoint
            );

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
                        debug!("[{}] Received shutdown signal, exiting processing loop", reaction_name);
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
                    debug!("[{}] Received empty result set from query", reaction_name);
                    continue;
                }

                let query_id = &query_result.query_id;
                trace!("Processing results for query_id: {}", query_id);

                if !last_query_id.is_empty()
                    && last_query_id != query_result.query_id
                    && !batch.is_empty()
                {
                    if client.is_none() || !matches!(connection_state, ConnectionState::Connected) {
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
                                error!("State transition: Connecting -> Failed (attempt {}, total: {}): {}",
                                       consecutive_failures, _total_connection_attempts, e);
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
                                            if last_successful_send.elapsed()
                                                > Duration::from_secs(5)
                                            {
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
                                            "[{}] Failed to send batch (total failures: {}): {}",
                                            reaction_name, _failed_sends, e
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
                                        "Failed to create client (attempt {}, total: {}): {}",
                                        consecutive_failures, _total_connection_attempts, e
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
                                                    last_connection_attempt =
                                                        std::time::Instant::now();
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
                                            error!("[{}] Failed to send batch (total failures: {}): {}", reaction_name, _failed_sends, e);
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
                            error!(
                                "[{}] Failed to create client for final batch: {}",
                                reaction_name, e
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
                                    error!(
                                        "[{}] Failed to send final batch: {}",
                                        reaction_name, e
                                    );
                                    break;
                                }
                            }
                        } else {
                            break;
                        }
                    }
                }
            }

            info!("[{}] gRPC reaction processing task stopped", reaction_name);
            *status.write().await = ComponentStatus::Stopped;
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

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}
