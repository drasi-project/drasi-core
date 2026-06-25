// Copyright 2026 The Drasi Authors.
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

//! Adaptive batching runner.

use std::collections::HashMap;
use std::time::Duration;

use log::{debug, error, info, warn};
use tokio::sync::{mpsc, oneshot};
use tonic::transport::Channel;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::reactions::common::{base::ReactionBase, AdaptiveBatchConfig};

use crate::adaptive_batcher::{AdaptiveBatcher, AdaptiveBatcherConfig};
use crate::batch::{merge_metadata, BatchKey};
use crate::config::GrpcReactionConfig;
use crate::connection::create_client_with_retry;
use crate::proto::{ProtoQueryResultItem, ReactionServiceClient};
use crate::send::send_batch_with_retry;
use crate::templates::{build_proto_item, QueryEmissionContext, TemplateEngine};

type BatcherItem = (BatchKey, ProtoQueryResultItem);

pub(crate) struct AdaptiveRunnerParams {
    pub reaction_name: String,
    pub adaptive: AdaptiveBatchConfig,
    pub base: ReactionBase,
    pub config: GrpcReactionConfig,
    pub shutdown_rx: oneshot::Receiver<()>,
}

pub(crate) async fn run(params: AdaptiveRunnerParams) {
    let AdaptiveRunnerParams {
        reaction_name,
        adaptive,
        base,
        config,
        mut shutdown_rx,
    } = params;

    let endpoint = config.endpoint.clone();
    let base_metadata = config.metadata.clone();
    let max_retries = config.max_retries;
    let timeout_ms = config.timeout_ms;
    let initial_connection_timeout_ms = config.initial_connection_timeout_ms;
    let connection_retry_attempts = config.connection_retry_attempts;
    let status_handle = base.status_handle();

    let runtime_cfg = AdaptiveBatcherConfig {
        min_batch_size: adaptive.adaptive_min_batch_size,
        max_batch_size: adaptive.adaptive_max_batch_size,
        throughput_window: Duration::from_millis(adaptive.adaptive_window_size as u64 * 100),
        max_wait_time: Duration::from_millis(adaptive.adaptive_batch_timeout_ms),
        min_wait_time: Duration::from_millis(100)
            .min(Duration::from_millis(adaptive.adaptive_batch_timeout_ms)),
        adaptive_enabled: true,
    };

    let batch_channel_capacity = runtime_cfg.recommended_channel_capacity();
    let (batch_tx, batch_rx) = mpsc::channel::<BatcherItem>(batch_channel_capacity);
    debug!(
        "Adaptive runner using batch channel capacity: {} (max_batch_size: {} × 5)",
        batch_channel_capacity, runtime_cfg.max_batch_size
    );

    let batcher_endpoint = endpoint.clone();
    let batcher_name = reaction_name.clone();

    let template_engine = config
        .output_templates
        .as_ref()
        .filter(|t| t.has_renderable_templates())
        .map(|_| TemplateEngine::new());

    let mut batcher_handle = tokio::spawn(async move {
        let mut batcher = AdaptiveBatcher::new(batch_rx, runtime_cfg);
        let mut client: Option<ReactionServiceClient<Channel>> = None;
        let mut successful_sends = 0u64;
        let mut failed_sends = 0u64;

        info!("Adaptive gRPC reaction starting for endpoint: {batcher_endpoint} (lazy connection)");

        while let Some(batch) = batcher.next_batch().await {
            if batch.is_empty() {
                continue;
            }

            debug!("Adaptive batch ready with {} items", batch.len());

            if client.is_none() {
                match create_client_with_retry(
                    &batcher_endpoint,
                    initial_connection_timeout_ms,
                    connection_retry_attempts,
                )
                .await
                {
                    Ok(c) => {
                        info!("Successfully created gRPC client for endpoint: {batcher_endpoint}");
                        client = Some(c);
                    }
                    Err(e) => {
                        // Could not establish a connection. Drop this batch and
                        // keep running; the connection is retried on the next
                        // batch. Favors uptime over completeness.
                        failed_sends += 1;
                        warn!(
                            "[{batcher_name}] Failed to create gRPC client: {e}; dropping {} \
                             batched item(s) and continuing (will retry on the next batch)",
                            batch.len()
                        );
                        continue;
                    }
                }
            }

            let mut batches_by_key: HashMap<BatchKey, Vec<ProtoQueryResultItem>> = HashMap::new();
            for (key, item) in batch {
                batches_by_key.entry(key).or_default().push(item);
            }

            for (key, items) in batches_by_key.into_iter() {
                let mut retry_swaps = 0u32;
                while let Some(c) = client.as_mut() {
                    match send_batch_with_retry(
                        c,
                        items.clone(),
                        &key.query_id,
                        &key.metadata_map(),
                        max_retries,
                        &batcher_endpoint,
                        timeout_ms,
                    )
                    .await
                    {
                        Ok((needs_new_client, new_client)) => {
                            if needs_new_client {
                                if let Some(nc) = new_client {
                                    client = Some(nc);
                                    retry_swaps += 1;
                                    if retry_swaps <= 2 {
                                        continue;
                                    }
                                } else {
                                    client = None;
                                }
                                // Transient delivery failure: drop this sub-batch
                                // and keep running. The connection is retried on
                                // the next batch.
                                failed_sends += 1;
                                warn!(
                                    "[{batcher_name}] Failed to deliver adaptive batch of {} \
                                     item(s) for query '{}' after retries; dropping and continuing",
                                    items.len(),
                                    key.query_id
                                );
                                break;
                            }

                            successful_sends += 1;
                            if successful_sends.is_multiple_of(100) {
                                info!(
                                    "Adaptive metrics - Successful: {successful_sends}, Failed: {failed_sends}"
                                );
                            }
                            break;
                        }
                        Err(e) => {
                            // Permanent delivery failure (downstream rejected the
                            // batch after retries): drop this sub-batch and keep
                            // running rather than stopping the reaction.
                            failed_sends += 1;
                            error!(
                                "[{batcher_name}] Failed to deliver adaptive batch for query \
                                 '{}': {e}; dropping and continuing",
                                key.query_id
                            );
                            break;
                        }
                    }
                }
            }
        }

        info!(
            "[{batcher_name}] Adaptive batcher task completed - Successful: {successful_sends}, Failed: {failed_sends}"
        );
    });

    let priority_queue = base.priority_queue.clone();
    let cfg_for_loop = config;
    let engine_for_loop = template_engine;

    loop {
        let query_result = tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                debug!("[{reaction_name}] Received shutdown signal, exiting adaptive processing loop");
                break;
            }
            result = priority_queue.dequeue() => result,
        };

        if !matches!(status_handle.get_status().await, ComponentStatus::Running) {
            info!(
                "[{reaction_name}] Reaction status changed to non-running, exiting adaptive loop"
            );
            return;
        }

        if query_result.results.is_empty() {
            debug!("[{reaction_name}] Received empty result set from query");
            continue;
        }

        let query_id = query_result.query_id.clone();
        let emission = QueryEmissionContext {
            query_id: &query_id,
            sequence: query_result.sequence,
            timestamp: query_result.timestamp,
            metadata: &query_result.metadata,
        };

        for diff in &query_result.results {
            if matches!(diff, drasi_lib::channels::ResultDiff::Noop) {
                debug!("[{reaction_name}] Ignoring noop result");
                continue;
            }
            let Some(built) =
                build_proto_item(&cfg_for_loop, engine_for_loop.as_ref(), &emission, diff)
            else {
                continue;
            };
            let effective_metadata = merge_metadata(&base_metadata, &built.request_metadata);
            let key = BatchKey::new(query_id.clone(), effective_metadata);
            if batch_tx.send((key, built.item)).await.is_err() {
                let msg = "failed to send item to adaptive batcher".to_string();
                error!("[{reaction_name}] {msg}");
                status_handle
                    .set_status(ComponentStatus::Error, Some(msg))
                    .await;
                return;
            }
        }
    }

    drop(batch_tx);
    if tokio::time::timeout(Duration::from_millis(1500), &mut batcher_handle)
        .await
        .is_err()
    {
        warn!(
            "[{reaction_name}] Adaptive batcher did not finish within the shutdown window — \
             in-flight batch abandoned"
        );
        batcher_handle.abort();
    }

    info!("[{reaction_name}] Adaptive gRPC reaction completed");
    status_handle
        .set_status(
            ComponentStatus::Stopped,
            Some("Adaptive gRPC reaction stopped".to_string()),
        )
        .await;
}
