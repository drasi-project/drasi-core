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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, warn};
use tokio::sync::{mpsc, oneshot};
use tonic::transport::Channel;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::reactions::common::{base::ReactionBase, AdaptiveBatchConfig};
use drasi_lib::ReactionRecoveryPolicy;

use crate::adaptive_batcher::{AdaptiveBatcher, AdaptiveBatcherConfig};
use crate::batch::{merge_metadata, BatchKey};
use crate::config::GrpcReactionConfig;
use crate::connection::create_client_with_retry;
use crate::proto::{ProtoQueryResultItem, ReactionServiceClient};
use crate::send::send_batch_with_retry;
use crate::templates::{build_proto_item, QueryEmissionContext, TemplateEngine};
use drasi_lib::reactions::common::{batch_checkpoint_candidates, CheckpointState, FailureAction};

/// A batcher item: routing key, proto payload, and whether this item is the
/// terminal (last) item of its originating `QueryResult`. The terminal flag
/// keeps checkpoint advancement at-least-once correct when a `QueryResult` is
/// split across batches — the sequence only advances once its terminal acks.
type BatcherItem = (BatchKey, ProtoQueryResultItem, bool);

pub(crate) struct AdaptiveRunnerParams {
    pub reaction_name: String,
    pub adaptive: AdaptiveBatchConfig,
    pub base: ReactionBase,
    pub config: GrpcReactionConfig,
    pub shutdown_rx: oneshot::Receiver<()>,
    pub checkpoints: CheckpointState,
    pub policy: ReactionRecoveryPolicy,
}

pub(crate) async fn run(params: AdaptiveRunnerParams) {
    let AdaptiveRunnerParams {
        reaction_name,
        adaptive,
        base,
        config,
        mut shutdown_rx,
        checkpoints,
        policy,
    } = params;

    let endpoint = config.endpoint.clone();
    let base_metadata = config.metadata.clone();
    let max_retries = config.max_retries;
    let timeout_ms = config.timeout_ms;
    let initial_connection_timeout_ms = config.initial_connection_timeout_ms;
    let connection_retry_attempts = config.connection_retry_attempts;
    let status_handle = base.status_handle();

    // Set when the batcher fail-stops, so the main loop does not overwrite the
    // `Error` status with `Stopped` on its way out.
    let errored = Arc::new(AtomicBool::new(false));

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
    let batcher_base = base.clone_shared();
    let batcher_errored = errored.clone();

    let template_engine = config
        .output_templates
        .as_ref()
        .filter(|t| t.has_renderable_templates())
        .map(|_| TemplateEngine::new());

    let mut batcher_handle = tokio::spawn(async move {
        let mut checkpoints = checkpoints;
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

            // Per-query checkpoint candidates plus the routing-key sub-batches.
            // See `batch_checkpoint_candidates` for the `completed` / `seen`
            // at-least-once rationale.
            let mut meta = Vec::with_capacity(batch.len());
            let mut batches_by_key: HashMap<BatchKey, Vec<ProtoQueryResultItem>> = HashMap::new();
            for (key, item, is_terminal) in batch {
                meta.push((key.query_id.clone(), item.sequence, is_terminal));
                batches_by_key.entry(key).or_default().push(item);
            }
            let (completed, seen) = batch_checkpoint_candidates(meta);

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
                    Err(e) => match FailureAction::from_policy(policy) {
                        FailureAction::Stop => {
                            fail_stop(
                                &batcher_base,
                                &batcher_errored,
                                &batcher_name,
                                &format!(
                                    "gRPC connection failed: {e}; stopped per recovery policy — \
                                     the batch replays from the outbox on restart"
                                ),
                            )
                            .await;
                            return;
                        }
                        FailureAction::SkipAndContinue => {
                            failed_sends += 1;
                            warn!(
                                "[{batcher_name}] Failed to create gRPC client: {e}; skipping \
                                 batch and advancing checkpoints per AutoSkipGap recovery policy"
                            );
                            for (q, s) in &seen {
                                let _ = checkpoints.advance(&batcher_base, q, *s).await;
                            }
                            continue;
                        }
                    },
                }
            }

            // Queries that had a dropped sub-batch under AutoSkipGap. Under
            // Strict the loop returns on the first failure, so this stays empty.
            let mut failed_queries: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for (key, items) in batches_by_key.into_iter() {
                let delivered = deliver_sub_batch(
                    &mut client,
                    &items,
                    &key,
                    max_retries,
                    &batcher_endpoint,
                    timeout_ms,
                    &batcher_name,
                )
                .await;

                if delivered {
                    successful_sends += 1;
                    if successful_sends % 100 == 0 {
                        info!(
                            "Adaptive metrics - Successful: {successful_sends}, Failed: {failed_sends}"
                        );
                    }
                } else {
                    failed_sends += 1;
                    match FailureAction::from_policy(policy) {
                        FailureAction::Stop => {
                            fail_stop(
                                &batcher_base,
                                &batcher_errored,
                                &batcher_name,
                                &format!(
                                    "gRPC delivery failed for query '{}'; stopped per recovery \
                                     policy — the batch replays from the outbox on restart",
                                    key.query_id
                                ),
                            )
                            .await;
                            return;
                        }
                        FailureAction::SkipAndContinue => {
                            warn!(
                                "[{batcher_name}] Dropping sub-batch for query '{}' and continuing \
                                 per AutoSkipGap recovery policy",
                                key.query_id
                            );
                            failed_queries.insert(key.query_id.clone());
                        }
                    }
                }
            }

            // Advance checkpoints after acks. Fully-acked queries advance to
            // their terminal max. Queries with a dropped sub-batch (AutoSkipGap
            // only) advance to their `seen` max, accepting the loss — this can
            // skip the un-acked tail of a result split across batches, which is
            // the policy's liveness-over-completeness contract.
            let mut to_advance = completed;
            for q in &failed_queries {
                if let Some(s) = seen.get(q) {
                    let e = to_advance.entry(q.clone()).or_insert(0);
                    *e = (*e).max(*s);
                }
            }
            for (q, s) in &to_advance {
                if let Err(e) = checkpoints.advance(&batcher_base, q, *s).await {
                    error!(
                        "[{batcher_name}] Failed to write checkpoint for query '{q}' (seq {s}): {e}"
                    );
                    if FailureAction::from_policy(policy) == FailureAction::Stop {
                        fail_stop(
                            &batcher_base,
                            &batcher_errored,
                            &batcher_name,
                            &format!(
                                "gRPC checkpoint write failed for query '{q}' (seq {s}); \
                                 stopped per recovery policy"
                            ),
                        )
                        .await;
                        return;
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
            // The batcher dropping its receiver (e.g. a fail-stop) closes this
            // sender; break promptly instead of blocking on the next dequeue.
            _ = batch_tx.closed() => {
                debug!("[{reaction_name}] Batcher exited; exiting adaptive processing loop");
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
        let seq = query_result.sequence;

        let emission = QueryEmissionContext {
            query_id: &query_id,
            sequence: seq,
            timestamp: query_result.timestamp,
            metadata: &query_result.metadata,
        };

        // Build all items first so the last one can be flagged terminal; the
        // checkpoint only advances once a result's terminal item is acked.
        let mut built_items: Vec<(BatchKey, ProtoQueryResultItem)> = Vec::new();
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
            built_items.push((key, built.item));
        }

        let last_idx = built_items.len().saturating_sub(1);
        for (i, (key, item)) in built_items.into_iter().enumerate() {
            let is_terminal = i == last_idx;
            if batch_tx.send((key, item, is_terminal)).await.is_err() {
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

    // Preserve the `Error` status set by a fail-stopping batcher.
    if errored.load(Ordering::SeqCst) {
        info!("[{reaction_name}] Adaptive gRPC reaction stopped after a delivery failure");
        return;
    }

    info!("[{reaction_name}] Adaptive gRPC reaction completed");
    status_handle
        .set_status(
            ComponentStatus::Stopped,
            Some("Adaptive gRPC reaction stopped".to_string()),
        )
        .await;
}

/// Set the reaction to `Error` and record that a fail-stop occurred so the main
/// loop does not overwrite the status with `Stopped`.
async fn fail_stop(base: &ReactionBase, errored: &Arc<AtomicBool>, reaction_name: &str, msg: &str) {
    error!("[{reaction_name}] {msg}");
    errored.store(true, Ordering::SeqCst);
    base.status_handle()
        .set_status(ComponentStatus::Error, Some(msg.to_string()))
        .await;
}

/// Deliver one routing-key sub-batch, encapsulating the connection retry/swap
/// loop. Returns `true` when the sub-batch was acked, `false` on a sustained
/// failure (the caller then applies the recovery policy).
async fn deliver_sub_batch(
    client: &mut Option<ReactionServiceClient<Channel>>,
    items: &[ProtoQueryResultItem],
    key: &BatchKey,
    max_retries: u32,
    endpoint: &str,
    timeout_ms: u64,
    reaction_name: &str,
) -> bool {
    let mut retry_swaps = 0u32;
    loop {
        let Some(c) = client.as_mut() else {
            return false;
        };
        match send_batch_with_retry(
            c,
            items.to_vec(),
            &key.query_id,
            &key.metadata_map(),
            max_retries,
            endpoint,
            timeout_ms,
        )
        .await
        {
            Ok((needs_new_client, new_client)) => {
                if needs_new_client {
                    if let Some(nc) = new_client {
                        *client = Some(nc);
                        retry_swaps += 1;
                        if retry_swaps <= 2 {
                            continue;
                        }
                    } else {
                        *client = None;
                    }
                    warn!(
                        "[{reaction_name}] Failed to deliver adaptive batch of {} item(s) for \
                         query '{}' after retries",
                        items.len(),
                        key.query_id
                    );
                    return false;
                }
                return true;
            }
            Err(e) => {
                error!(
                    "[{reaction_name}] Failed to deliver adaptive batch for query '{}': {e}",
                    key.query_id
                );
                return false;
            }
        }
    }
}
