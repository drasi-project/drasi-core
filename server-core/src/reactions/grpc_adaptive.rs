use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
// RecvError no longer needed with trait-based receivers
use tokio::sync::{mpsc, RwLock};
use tonic::transport::Channel;

use crate::channels::priority_queue::PriorityQueue;
use crate::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, QueryResult,
};
use crate::config::ReactionConfig;
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
#[derive(Clone)]
pub struct AdaptiveGrpcReaction {
    config: ReactionConfig,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: ComponentEventSender,
    endpoint: String,
    timeout_ms: u64,
    max_retries: u32,
    metadata: HashMap<String, String>,
    _connection_retry_attempts: u32,
    initial_connection_timeout_ms: u64,
    // Adaptive batching configuration
    adaptive_config: AdaptiveBatchConfig,
    // Subscription management
    subscription_tasks: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
    priority_queue: PriorityQueue<QueryResult>,
    processing_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl AdaptiveGrpcReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        // Extract gRPC-specific configuration
        let endpoint = config
            .properties
            .get("endpoint")
            .and_then(|v| v.as_str())
            .unwrap_or("grpc://localhost:50052")
            .to_string();

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

        // Configure adaptive batching
        let mut adaptive_config = AdaptiveBatchConfig::default();

        // Allow overriding adaptive parameters from config
        if let Some(max_batch) = config
            .properties
            .get("adaptive_max_batch_size")
            .and_then(|v| v.as_u64())
        {
            adaptive_config.max_batch_size = max_batch as usize;
        }
        if let Some(min_batch) = config
            .properties
            .get("adaptive_min_batch_size")
            .and_then(|v| v.as_u64())
        {
            adaptive_config.min_batch_size = min_batch as usize;
        }
        if let Some(max_wait_ms) = config
            .properties
            .get("adaptive_max_wait_ms")
            .and_then(|v| v.as_u64())
        {
            adaptive_config.max_wait_time = Duration::from_millis(max_wait_ms);
        }
        if let Some(min_wait_ms) = config
            .properties
            .get("adaptive_min_wait_ms")
            .and_then(|v| v.as_u64())
        {
            adaptive_config.min_wait_time = Duration::from_millis(min_wait_ms);
        }
        if let Some(window_secs) = config
            .properties
            .get("adaptive_window_secs")
            .and_then(|v| v.as_u64())
        {
            adaptive_config.throughput_window = Duration::from_secs(window_secs);
        }

        // Check if adaptive mode is explicitly disabled
        if let Some(enabled) = config
            .properties
            .get("adaptive_enabled")
            .and_then(|v| v.as_bool())
        {
            adaptive_config.adaptive_enabled = enabled;
        }

        Self {
            config: config.clone(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
            endpoint,
            timeout_ms,
            max_retries,
            metadata,
            _connection_retry_attempts: connection_retry_attempts,
            initial_connection_timeout_ms,
            adaptive_config,
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            priority_queue: PriorityQueue::new(config.priority_queue_capacity.unwrap_or(10000)),
            processing_task: Arc::new(RwLock::new(None)),
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
        priority_queue: PriorityQueue<QueryResult>,
        status: Arc<RwLock<ComponentStatus>>,
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
                let query_result = priority_queue.dequeue().await;

                if !matches!(*status.read().await, ComponentStatus::Running) {
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
        info!("Starting adaptive gRPC reaction: {}", self.config.id);

        *self.status.write().await = ComponentStatus::Starting;

        // Send start event
        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting adaptive gRPC reaction".to_string()),
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

            let mut receiver = subscription_response.receiver;
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
                    match receiver.recv().await {
                        Ok(query_result) => {
                            // Dereference Arc to get QueryResult and enqueue
                            if !priority_queue.enqueue(Arc::clone(&query_result)).await {
                                warn!(
                                    "[{}] Priority queue full, dropped result from query '{}'",
                                    reaction_id, query_id_clone
                                );
                            }
                        }
                        Err(e) => {
                            // Check if it's a lag error or closed channel
                            let error_str = e.to_string();
                            if error_str.contains("lagged") {
                                warn!(
                                    "[{}] Receiver lagged for query '{}': {}",
                                    reaction_id, query_id_clone, error_str
                                );
                                continue;
                            } else {
                                info!(
                                    "[{}] Receiver error for query '{}', forwarder exiting: {}",
                                    reaction_id, query_id_clone, error_str
                                );
                                break;
                            }
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
            message: Some("Adaptive gRPC reaction started".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Start processing task that dequeues from priority queue
        let reaction_name = self.config.id.clone();
        let status = Arc::clone(&self.status);
        let endpoint = self.endpoint.clone();
        let max_retries = self.max_retries;
        let metadata = self.metadata.clone();
        let timeout_ms = self.timeout_ms;
        let initial_connection_timeout_ms = self.initial_connection_timeout_ms;
        let adaptive_config = self.adaptive_config.clone();
        let priority_queue = self.priority_queue.clone();

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
                priority_queue,
                status,
            )
            .await;
        });

        // Store the processing task handle
        *self.processing_task.write().await = Some(processing_task_handle);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping adaptive gRPC reaction", self.config.id);

        *self.status.write().await = ComponentStatus::Stopping;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Stopping,
            timestamp: chrono::Utc::now(),
            message: Some("Stopping adaptive gRPC reaction".to_string()),
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
            message: Some("Adaptive gRPC reaction stopped successfully".to_string()),
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
