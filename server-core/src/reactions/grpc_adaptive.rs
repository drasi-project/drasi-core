use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
// RecvError no longer needed with trait-based receivers
use tokio::sync::mpsc;
use tonic::transport::Channel;

use crate::channels::{ComponentEventSender, ComponentStatus};
use crate::config::ReactionConfig;
use crate::reactions::base::ReactionBase;
use crate::reactions::Reaction;
use crate::server_core::DrasiServerCore;
use crate::utils::{AdaptiveBatchConfig, AdaptiveBatcher};

// Use the same proto module from the original grpc module
use super::grpc::proto::{
    reaction_service_client::ReactionServiceClient, ProcessResultsRequest,
    QueryResult as ProtoQueryResult, QueryResultItem as ProtoQueryResultItem,
};

use super::grpc::convert_json_to_proto_struct;

/// Adaptive gRPC reaction that dynamically adjusts batching based on throughput
pub struct AdaptiveGrpcReaction {
    base: ReactionBase,
    endpoint: String,
    timeout_ms: u64,
    max_retries: u32,
    metadata: HashMap<String, String>,
    _connection_retry_attempts: u32,
    initial_connection_timeout_ms: u64,
    // Adaptive batching configuration
    adaptive_config: AdaptiveBatchConfig,
}

impl AdaptiveGrpcReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        // Extract gRPC-specific configuration from typed config
        let (
            endpoint,
            timeout_ms,
            max_retries,
            connection_retry_attempts,
            initial_connection_timeout_ms,
            metadata,
        ) = match &config.config {
            crate::config::ReactionSpecificConfig::Grpc(grpc_config) => (
                grpc_config.endpoint.clone(),
                grpc_config.timeout_ms,
                grpc_config.max_retries,
                grpc_config.connection_retry_attempts,
                grpc_config.initial_connection_timeout_ms,
                grpc_config.metadata.clone(),
            ),
            _ => (
                "grpc://localhost:50052".to_string(),
                5000u64,
                5u32,
                5u32,
                10000u64,
                HashMap::new(),
            ),
        };

        // Configure adaptive batching
        let mut adaptive_config = AdaptiveBatchConfig::default();

        // Allow overriding adaptive parameters from config if using custom variant
        if let crate::config::ReactionSpecificConfig::Custom { properties } = &config.config {
            if let Some(max_batch) = properties
                .get("adaptive_max_batch_size")
                .and_then(|v| v.as_u64())
            {
                adaptive_config.max_batch_size = max_batch as usize;
            }
            if let Some(min_batch) = properties
                .get("adaptive_min_batch_size")
                .and_then(|v| v.as_u64())
            {
                adaptive_config.min_batch_size = min_batch as usize;
            }
            if let Some(max_wait_ms) = properties
                .get("adaptive_max_wait_ms")
                .and_then(|v| v.as_u64())
            {
                adaptive_config.max_wait_time = Duration::from_millis(max_wait_ms);
            }
            if let Some(min_wait_ms) = properties
                .get("adaptive_min_wait_ms")
                .and_then(|v| v.as_u64())
            {
                adaptive_config.min_wait_time = Duration::from_millis(min_wait_ms);
            }
            if let Some(window_secs) = properties
                .get("adaptive_window_secs")
                .and_then(|v| v.as_u64())
            {
                adaptive_config.throughput_window = Duration::from_secs(window_secs);
            }

            // Check if adaptive mode is explicitly disabled
            if let Some(enabled) = properties.get("adaptive_enabled").and_then(|v| v.as_bool()) {
                adaptive_config.adaptive_enabled = enabled;
            }
        }

        Self {
            base: ReactionBase::new(config, event_tx),
            endpoint,
            timeout_ms,
            max_retries,
            metadata,
            _connection_retry_attempts: connection_retry_attempts,
            initial_connection_timeout_ms,
            adaptive_config,
        }
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

    async fn run_internal(
        reaction_name: String,
        endpoint: String,
        metadata: HashMap<String, String>,
        timeout_ms: u64,
        max_retries: u32,
        _connection_retry_attempts: u32,
        initial_connection_timeout_ms: u64,
        adaptive_config: AdaptiveBatchConfig,
        base: ReactionBase,
    ) {
        let _timeout_ms = timeout_ms;

        // Create channel for batching
        let (batch_tx, batch_rx) = mpsc::channel(1000);

        // Spawn adaptive batcher task
        let batcher_handle = tokio::spawn(async move {
            let mut batcher = AdaptiveBatcher::new(batch_rx, adaptive_config);
            let mut client: Option<ReactionServiceClient<Channel>> = None;
            let mut consecutive_failures = 0u32;
            let mut successful_sends = 0u64;
            let mut failed_sends = 0u64;

            info!(
                "Adaptive gRPC reaction starting for endpoint: {} (lazy connection)",
                endpoint
            );

            while let Some(batch) = batcher.next_batch().await {
                if batch.is_empty() {
                    continue;
                }

                debug!("Adaptive batch ready with {} items", batch.len());

                // Ensure we have a client
                if client.is_none() {
                    match Self::create_client(&endpoint, initial_connection_timeout_ms).await {
                        Ok(c) => {
                            info!(
                                "Successfully created gRPC client for endpoint: {}",
                                endpoint
                            );
                            client = Some(c);
                            consecutive_failures = 0;
                        }
                        Err(e) => {
                            consecutive_failures += 1;
                            error!(
                                "Failed to create client (attempt {}): {}",
                                consecutive_failures, e
                            );

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
                        for (query_id, items) in batches_by_query {
                            let mut retries = 0;
                            let mut sent = false;

                            while !sent && retries <= max_retries {
                                match Self::send_batch(c, items.clone(), &query_id, &metadata).await
                                {
                                    Ok(_) => {
                                        sent = true;
                                        successful_sends += 1;
                                        consecutive_failures = 0;

                                        if successful_sends % 100 == 0 {
                                            info!(
                                                "Adaptive metrics - Successful: {}, Failed: {}",
                                                successful_sends, failed_sends
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        retries += 1;
                                        warn!(
                                            "Failed to send batch (retry {}/{}): {}",
                                            retries, max_retries, e
                                        );

                                        if retries > max_retries {
                                            failed_sends += 1;
                                            error!(
                                                "Failed to send batch after {} retries",
                                                max_retries
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
                "Adaptive batcher task completed - Successful: {}, Failed: {}",
                successful_sends, failed_sends
            );
        });

        // Main task: receive query results from priority queue and forward to batcher
        tokio::spawn(async move {
            let mut last_query_id = String::new();
            let mut current_batch = Vec::new();

            loop {
                // Dequeue from priority queue (blocking)
                let query_result = base.priority_queue.dequeue().await;

                if !matches!(*base.status.read().await, ComponentStatus::Running) {
                    info!(
                        "[{}] Reaction status changed to non-running, exiting",
                        reaction_name
                    );
                    break;
                }

                if query_result.results.is_empty() {
                    debug!("[{}] Received empty result set from query", reaction_name);
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

            info!("[{}] Adaptive gRPC reaction completed", reaction_name);
        });
    }
}

#[async_trait]
impl Reaction for AdaptiveGrpcReaction {
    async fn start(&self, server_core: Arc<DrasiServerCore>) -> Result<()> {
        info!("Starting adaptive gRPC reaction: {}", self.base.config.id);

        // Set status to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting adaptive gRPC reaction".to_string()),
            )
            .await?;

        // Subscribe to queries
        self.base.subscribe_to_queries(server_core).await?;

        // Set status to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Adaptive gRPC reaction started".to_string()),
            )
            .await?;

        // Start processing task that dequeues from priority queue
        let reaction_name = self.base.config.id.clone();
        let endpoint = self.endpoint.clone();
        let max_retries = self.max_retries;
        let metadata = self.metadata.clone();
        let timeout_ms = self.timeout_ms;
        let initial_connection_timeout_ms = self.initial_connection_timeout_ms;
        let adaptive_config = self.adaptive_config.clone();
        let base = ReactionBase {
            config: self.base.config.clone(),
            status: self.base.status.clone(),
            event_tx: self.base.event_tx.clone(),
            priority_queue: self.base.priority_queue.clone(),
            subscription_tasks: self.base.subscription_tasks.clone(),
            processing_task: self.base.processing_task.clone(),
        };

        let processing_task_handle = tokio::spawn(async move {
            Self::run_internal(
                reaction_name,
                endpoint,
                metadata,
                timeout_ms,
                max_retries,
                0, // We'll handle connection retry inside run_internal
                initial_connection_timeout_ms,
                adaptive_config,
                base,
            )
            .await;
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_task_handle).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping adaptive gRPC reaction", self.base.config.id);

        // Set status to Stopping
        self.base
            .set_status_with_event(
                ComponentStatus::Stopping,
                Some("Stopping adaptive gRPC reaction".to_string()),
            )
            .await?;

        // Perform common cleanup
        self.base.stop_common().await?;

        // Wait a moment for cleanup
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Set status to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Adaptive gRPC reaction stopped successfully".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    fn get_config(&self) -> &ReactionConfig {
        &self.base.config
    }
}
