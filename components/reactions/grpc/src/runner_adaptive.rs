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
//!
//! Uses the throughput-aware [`crate::adaptive_batcher::AdaptiveBatcher`]
//! to size batches dynamically between configured minimum and maximum
//! bounds. Items are forwarded through an mpsc channel to a dedicated
//! batcher task that emits ready-to-send batches as soon as either the
//! adaptive size or wait-time criteria are met.

use std::collections::HashMap;
use std::time::Duration;

use log::{debug, error, info, warn};
use tokio::sync::{mpsc, oneshot};
use tonic::transport::Channel;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::reactions::common::{base::ReactionBase, AdaptiveBatchConfig as LibAdaptiveConfig};

use crate::adaptive_batcher::{AdaptiveBatchConfig, AdaptiveBatcher};
use crate::config::GrpcReactionConfig;
use crate::connection::create_client_with_retry;
use crate::proto::{ProtoQueryResultItem, ReactionServiceClient};
use crate::send::send_batch_with_retry;
use crate::templates::{build_proto_item, TemplateEngine};

pub(crate) struct AdaptiveRunnerParams {
    pub reaction_name: String,
    pub adaptive: LibAdaptiveConfig,
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
    let metadata = config.metadata.clone();
    let max_retries = config.max_retries;
    let timeout_ms = config.timeout_ms;
    let initial_connection_timeout_ms = config.initial_connection_timeout_ms;
    let connection_retry_attempts = config.connection_retry_attempts;
    let status_handle = base.status_handle();

    // Convert the public lib config into the runtime batcher config.
    let runtime_cfg = AdaptiveBatchConfig {
        min_batch_size: adaptive.adaptive_min_batch_size,
        max_batch_size: adaptive.adaptive_max_batch_size,
        throughput_window: Duration::from_millis(adaptive.adaptive_window_size as u64 * 100),
        max_wait_time: Duration::from_millis(adaptive.adaptive_batch_timeout_ms),
        min_wait_time: Duration::from_millis(100),
        adaptive_enabled: true,
    };

    let batch_channel_capacity = runtime_cfg.recommended_channel_capacity();
    let (batch_tx, batch_rx) = mpsc::channel(batch_channel_capacity);
    debug!(
        "Adaptive runner using batch channel capacity: {} (max_batch_size: {} × 5)",
        batch_channel_capacity, runtime_cfg.max_batch_size
    );

    let batcher_endpoint = endpoint.clone();
    let batcher_metadata = metadata.clone();
    let batcher_name = reaction_name.clone();

    // Sender task: consume `ResultDiff`s from the priority queue, render
    // proto items, group by query, and ship to the batcher channel.
    let template_engine = config
        .output_templates
        .as_ref()
        .map(|_| TemplateEngine::new());

    let batcher_handle = tokio::spawn(async move {
        let mut batcher = AdaptiveBatcher::new(batch_rx, runtime_cfg);
        let mut client: Option<ReactionServiceClient<Channel>> = None;
        let mut consecutive_failures = 0u32;
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
                        consecutive_failures = 0;
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        error!("Failed to create client (attempt {consecutive_failures}): {e}");
                        let backoff =
                            Duration::from_millis(100 * 2u64.pow(consecutive_failures.min(10)));
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                }
            }

            // Group items by query_id
            let mut batches_by_query: HashMap<String, Vec<ProtoQueryResultItem>> = HashMap::new();
            for (query_id, items) in batch {
                batches_by_query.entry(query_id).or_default().extend(items);
            }

            for (query_id, items) in batches_by_query.into_iter() {
                let mut needs_swap = false;
                loop {
                    let Some(c) = client.as_mut() else { break };
                    match send_batch_with_retry(
                        c,
                        items.clone(),
                        &query_id,
                        &batcher_metadata,
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
                                    consecutive_failures = 0;
                                    continue;
                                } else {
                                    needs_swap = true;
                                    break;
                                }
                            } else {
                                successful_sends += 1;
                                if successful_sends % 100 == 0 {
                                    info!(
                                        "Adaptive metrics - Successful: {successful_sends}, Failed: {failed_sends}"
                                    );
                                }
                                break;
                            }
                        }
                        Err(e) => {
                            failed_sends += 1;
                            warn!(
                                "[{batcher_name}] Failed to send adaptive batch for query '{query_id}': {e}"
                            );
                            needs_swap = true;
                            break;
                        }
                    }
                }
                if needs_swap {
                    client = None;
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
    let mut last_query_id = String::new();
    let mut current_batch: Vec<ProtoQueryResultItem> = Vec::new();

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
            break;
        }

        if query_result.results.is_empty() {
            debug!("[{reaction_name}] Received empty result set from query");
            continue;
        }

        let query_id = query_result.query_id.clone();

        if !last_query_id.is_empty()
            && last_query_id != query_id
            && !current_batch.is_empty()
            && batch_tx
                .send((last_query_id.clone(), std::mem::take(&mut current_batch)))
                .await
                .is_err()
        {
            error!("[{reaction_name}] Failed to send batch to adaptive batcher");
            break;
        }
        last_query_id = query_id.clone();

        for diff in &query_result.results {
            let proto_item =
                build_proto_item(&cfg_for_loop, engine_for_loop.as_ref(), &query_id, diff);
            current_batch.push(proto_item);
        }

        // Forward early if we accumulated a lot for a single query.
        if current_batch.len() >= 100
            && batch_tx
                .send((last_query_id.clone(), std::mem::take(&mut current_batch)))
                .await
                .is_err()
        {
            error!("[{reaction_name}] Failed to send batch to adaptive batcher");
            break;
        }
    }

    if !current_batch.is_empty() && !last_query_id.is_empty() {
        let _ = batch_tx.send((last_query_id, current_batch)).await;
    }

    drop(batch_tx);
    let _ = batcher_handle.await;

    info!("[{reaction_name}] Adaptive gRPC reaction completed");
    status_handle
        .set_status(
            ComponentStatus::Stopped,
            Some("Adaptive gRPC reaction stopped".to_string()),
        )
        .await;
}
