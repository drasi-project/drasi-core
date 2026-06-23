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

//! Adaptive coalesced delivery loop. Forwards inbound diffs to the
//! [`AdaptiveBatcher`], then POSTs each coalesced batch as a single
//! Pattern C payload to the configured `batch_endpoint`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use log::{debug, error, info, warn};
use reqwest::Client;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::reactions::common::base::ReactionBase;
use drasi_lib::ReactionRecoveryPolicy;

use crate::adaptive_batcher::{AdaptiveBatcher, AdaptiveBatcherConfig};
use crate::batch::send_coalesced_batch;
use crate::checkpoint::{batch_checkpoint_candidates, CheckpointState, FailureAction};
use crate::config::HttpReactionConfig;
use crate::output::DefaultChangeNotification;
use crate::process::{build_handlebars, render_batch_item};

/// One coalesced batch item: the rendered wire payload plus the identity needed
/// to drive checkpoint advancement (`query_id`, `sequence`) and the
/// `is_terminal` flag marking the last item of its originating `QueryResult`.
/// A sequence's checkpoint only advances once its terminal item is acked, which
/// keeps a `QueryResult` split across batches at-least-once correct.
pub(crate) struct BatchItem {
    pub payload: serde_json::Value,
    pub query_id: String,
    pub sequence: u64,
    pub is_terminal: bool,
}

/// Run the adaptive (coalesced) processing loop. Spawns an internal
/// batcher task that lives for the lifetime of this loop.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_adaptive_loop(
    reaction_name: String,
    base: ReactionBase,
    config: HttpReactionConfig,
    runtime_adaptive: AdaptiveBatcherConfig,
    client: Client,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    checkpoints: CheckpointState,
    policy: ReactionRecoveryPolicy,
) {
    let status_handle = base.status_handle();
    let capacity = runtime_adaptive.recommended_channel_capacity();
    let (batch_tx, batch_rx) = tokio::sync::mpsc::channel::<BatchItem>(capacity);

    // Set when the batcher fail-stops, so the main loop does not overwrite the
    // `Error` status with `Stopped` on its way out.
    let errored = Arc::new(AtomicBool::new(false));

    debug!(
        "[{reaction_name}] HttpReaction adaptive mode using batch channel capacity: {capacity} (max_batch_size: {})",
        runtime_adaptive.max_batch_size
    );

    // Spawn batcher task. It owns the rx and exits when batch_tx is dropped.
    let mut batcher_handle = {
        let reaction_name = reaction_name.clone();
        let client = client.clone();
        let config = config.clone();
        let runtime_adaptive = runtime_adaptive.clone();
        let base = base.clone_shared();
        let errored = errored.clone();
        let mut checkpoints = checkpoints;
        tokio::spawn(async move {
            let mut batcher = AdaptiveBatcher::new(batch_rx, runtime_adaptive);
            let mut total_batches = 0u64;
            let mut total_results = 0u64;

            info!("[{reaction_name}] HTTP adaptive batcher started");

            while let Some(batch) = batcher.next_batch().await {
                if batch.is_empty() {
                    continue;
                }

                let batch_size = batch.len();
                total_results += batch_size as u64;
                total_batches += 1;

                debug!("[{reaction_name}] Processing adaptive batch of {batch_size} results");

                // Split the batch into wire payloads (moved, not cloned) and the
                // per-query checkpoint candidates (`completed` / `seen`). See
                // `batch_checkpoint_candidates` for the at-least-once rationale.
                let mut payloads = Vec::with_capacity(batch_size);
                let mut meta = Vec::with_capacity(batch_size);
                for item in batch {
                    meta.push((item.query_id, item.sequence, item.is_terminal));
                    payloads.push(item.payload);
                }
                let (completed, seen) = batch_checkpoint_candidates(meta);

                match deliver_batch(&client, &config, &reaction_name, payloads).await {
                    Ok(()) => {
                        // Advance each represented query to its max fully-acked
                        // (terminal) sequence after the side effect committed.
                        for (query_id, seq) in &completed {
                            if let Err(e) = checkpoints.advance(&base, query_id, *seq).await {
                                error!(
                                    "[{reaction_name}] Failed to write checkpoint for query \
                                     '{query_id}' (seq {seq}): {e}"
                                );
                                if FailureAction::from_policy(policy) == FailureAction::Stop {
                                    fail_stop(
                                        &base,
                                        &errored,
                                        &reaction_name,
                                        &format!(
                                            "HTTP checkpoint write failed for query '{query_id}' \
                                             (seq {seq}); stopped per recovery policy"
                                        ),
                                    )
                                    .await;
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => match FailureAction::from_policy(policy) {
                        FailureAction::Stop => {
                            error!("[{reaction_name}] Failed to deliver batch: {e}");
                            fail_stop(
                                &base,
                                &errored,
                                &reaction_name,
                                "HTTP batch delivery failed; stopped per recovery policy — \
                                 the batch replays from the outbox on restart",
                            )
                            .await;
                            return;
                        }
                        FailureAction::SkipAndContinue => {
                            warn!(
                                "[{reaction_name}] Failed to deliver batch: {e}; skipping and \
                                 advancing checkpoints per AutoSkipGap recovery policy"
                            );
                            // AutoSkipGap accepts loss: advance every represented
                            // query to its `seen` max so the dropped batch is not
                            // retried. This can skip the un-acked tail of a result
                            // split across batches — that is the policy's
                            // liveness-over-completeness contract.
                            for (query_id, seq) in &seen {
                                let _ = checkpoints.advance(&base, query_id, *seq).await;
                            }
                        }
                    },
                }

                if total_batches.is_multiple_of(100) {
                    info!(
                        "[{reaction_name}] Adaptive HTTP metrics - Batches: {total_batches}, Results: {total_results}, Avg batch size: {:.1}",
                        total_results as f64 / total_batches as f64
                    );
                }
            }

            info!(
                "[{reaction_name}] HTTP adaptive batcher stopped - Total batches: {total_batches}, Total results: {total_results}"
            );
        })
    };

    // Render templates once, off the delivery path; the batcher then only
    // coalesces already-rendered items.
    let handlebars = build_handlebars();

    // Main: pull from priority queue, render each item, forward to batcher
    loop {
        let query_result_arc = tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                debug!("[{reaction_name}] Received shutdown signal, exiting adaptive loop");
                break;
            }
            // The batcher dropping its receiver (e.g. a fail-stop) closes this
            // sender; break promptly instead of blocking on the next dequeue.
            _ = batch_tx.closed() => {
                debug!("[{reaction_name}] Batcher exited; exiting adaptive loop");
                break;
            }
            result = base.priority_queue.dequeue() => result,
        };
        let query_result = query_result_arc.as_ref();

        if query_result.results.is_empty() {
            continue;
        }

        let query_id = &query_result.query_id;
        let seq = query_result.sequence;

        // Convert this QueryResult's diffs into rendered batch items up-front
        // (dropping Noops). The batcher unit is one rendered item, so
        // adaptive_max_batch_size caps the number of payload items in each
        // BatchEnvelope. The last emitted item is flagged terminal so the
        // checkpoint only advances once the whole result has been acked.
        let notifications: Vec<DefaultChangeNotification> = query_result
            .results
            .iter()
            .filter_map(|d| DefaultChangeNotification::from_diff(query_result, d))
            .collect();
        let last_idx = notifications.len().saturating_sub(1);

        let mut batcher_closed = false;
        for (i, notification) in notifications.iter().enumerate() {
            let payload = render_batch_item(&handlebars, &config, notification, &reaction_name);
            let item = BatchItem {
                payload,
                query_id: query_id.clone(),
                sequence: seq,
                is_terminal: i == last_idx,
            };
            if batch_tx.send(item).await.is_err() {
                error!("[{reaction_name}] Failed to send to batch channel — batcher exited");
                batcher_closed = true;
                break;
            }
        }
        if batcher_closed {
            break;
        }
    }

    drop(batch_tx);
    match tokio::time::timeout(Duration::from_millis(1500), &mut batcher_handle).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            warn!("[{reaction_name}] Adaptive batcher task ended unexpectedly: {e}");
        }
        Err(_) => {
            warn!(
                "[{reaction_name}] Adaptive batcher did not finish within the 1.5 s shutdown window — \
                 aborting in-flight delivery"
            );
            batcher_handle.abort();
            let _ = batcher_handle.await;
        }
    }

    // Preserve the `Error` status set by a fail-stopping batcher.
    if errored.load(Ordering::SeqCst) {
        info!("[{reaction_name}] HTTP adaptive loop stopped after a delivery failure");
        return;
    }

    info!("[{reaction_name}] HTTP adaptive loop stopped");
    status_handle
        .set_status(
            ComponentStatus::Stopped,
            Some("HTTP reaction processing task stopped".to_string()),
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

/// POST the whole batch to the configured `batch_endpoint` as a single
/// Pattern C [`crate::output::BatchEnvelope`].
async fn deliver_batch(
    client: &Client,
    config: &HttpReactionConfig,
    reaction_name: &str,
    batch: Vec<serde_json::Value>,
) -> anyhow::Result<()> {
    let Some(batch_endpoint) = config.batch_endpoint.as_ref() else {
        anyhow::bail!("adaptive HTTP delivery requires batchEndpoint");
    };

    send_coalesced_batch(
        client,
        &config.base_url,
        batch_endpoint,
        &config.token,
        batch,
        reaction_name,
    )
    .await
}
