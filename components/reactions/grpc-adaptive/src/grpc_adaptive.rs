pub use super::config::GrpcAdaptiveReactionConfig;

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
// RecvError no longer needed with trait-based receivers
use tokio::sync::mpsc;
use tonic::transport::Channel;

use drasi_lib::channels::{ComponentEventSender, ComponentStatus, ResultDiff};
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::{QueryProvider, Reaction};

use crate::adaptive_batcher::{AdaptiveBatchConfig, AdaptiveBatcher};

/// Internal parameters for running the gRPC adaptive reaction
struct RunInternalParams {
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

// Use the proto module and helpers from the grpc-reaction plugin
use drasi_reaction_grpc::{
    convert_json_to_proto_struct, ProcessResultsRequest, ProtoQueryResult, ProtoQueryResultItem,
    ReactionServiceClient,
};

#[cfg(test)]
mod tests;

/// Adaptive gRPC reaction that dynamically adjusts batching based on throughput
pub struct AdaptiveGrpcReaction {
    base: ReactionBase,
    config: GrpcAdaptiveReactionConfig,
    endpoint: String,
    timeout_ms: u64,
    max_retries: u32,
    metadata: HashMap<String, String>,
    _connection_retry_attempts: u32,
    initial_connection_timeout_ms: u64,
    // Adaptive batching configuration
    adaptive_config: AdaptiveBatchConfig,
}

use super::GrpcAdaptiveReactionBuilder;

impl AdaptiveGrpcReaction {
    /// Create a builder for AdaptiveGrpcReaction
    pub fn builder(id: impl Into<String>) -> GrpcAdaptiveReactionBuilder {
        GrpcAdaptiveReactionBuilder::new(id)
    }

    /// Create a new adaptive gRPC reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: GrpcAdaptiveReactionConfig,
    ) -> Self {
        Self::create_internal(id.into(), queries, config, None, true)
    }

    /// Create from builder (internal method)
    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: GrpcAdaptiveReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        Self::create_internal(id, queries, config, priority_queue_capacity, auto_start)
    }

    /// Internal constructor
    fn create_internal(
        id: String,
        queries: Vec<String>,
        config: GrpcAdaptiveReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        // Convert from config adaptive fields to utils::AdaptiveBatchConfig
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
            endpoint: config.endpoint.clone(),
            timeout_ms: config.timeout_ms,
            max_retries: config.max_retries,
            metadata: config.metadata.clone(),
            _connection_retry_attempts: config.connection_retry_attempts,
            initial_connection_timeout_ms: config.initial_connection_timeout_ms,
            adaptive_config: utils_adaptive_config,
            config,
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
        params: RunInternalParams,
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
}

#[async_trait]
impl Reaction for AdaptiveGrpcReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "grpc_adaptive"
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
        props.insert(
            "max_retries".to_string(),
            serde_json::Value::Number(self.config.max_retries.into()),
        );
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
        info!("Starting adaptive gRPC reaction: {}", self.base.id);

        // Set status to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting adaptive gRPC reaction".to_string()),
            )
            .await?;

        // Subscribe to queries
        // QueryProvider is available from initialize() context
        self.base.subscribe_to_queries().await?;

        // Set status to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Adaptive gRPC reaction started".to_string()),
            )
            .await?;

        // Create shutdown channel for graceful termination
        let shutdown_rx = self.base.create_shutdown_channel().await;

        // Start processing task that dequeues from priority queue
        let reaction_name = self.base.id.clone();
        let endpoint = self.endpoint.clone();
        let max_retries = self.max_retries;
        let metadata = self.metadata.clone();
        let timeout_ms = self.timeout_ms;
        let initial_connection_timeout_ms = self.initial_connection_timeout_ms;
        let adaptive_config = self.adaptive_config.clone();
        let base = self.base.clone_shared();

        let processing_task_handle = tokio::spawn(async move {
            let params = RunInternalParams {
                reaction_name,
                endpoint,
                metadata,
                timeout_ms,
                max_retries,
                initial_connection_timeout_ms,
                adaptive_config,
                base,
            };
            Self::run_internal(params, shutdown_rx).await;
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_task_handle).await;

        Ok(())
    }

    async fn stop(&self) 

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
}
