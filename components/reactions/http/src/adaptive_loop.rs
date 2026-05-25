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
//! [`AdaptiveBatcher`], then either POSTs each coalesced batch as a
//! single payload (when `batch_endpoint` is set) or fans the batch back
//! out through the per-route [`process_result`] path.

use std::collections::HashMap;
use std::time::Duration;

use handlebars::Handlebars;
use log::{debug, error, info};
use reqwest::Client;

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::reactions::common::base::ReactionBase;

use crate::adaptive_batcher::{AdaptiveBatchConfig as RuntimeAdaptiveConfig, AdaptiveBatcher};
use crate::batch::send_coalesced_batch;
use crate::config::{
    synthesized_default_spec, HttpCallSpec, HttpReactionConfig, OperationType, TemplateRouting,
};
use crate::process::process_result;

/// Run the adaptive (coalesced) processing loop. Spawns an internal
/// batcher task that lives for the lifetime of this loop.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_adaptive_loop(
    reaction_name: String,
    base: ReactionBase,
    config: HttpReactionConfig,
    runtime_adaptive: RuntimeAdaptiveConfig,
    client: Client,
    handlebars: Handlebars<'static>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    let status_handle = base.status_handle();
    let capacity = runtime_adaptive.recommended_channel_capacity();
    let (batch_tx, batch_rx) = tokio::sync::mpsc::channel::<(String, Vec<ResultDiff>)>(capacity);

    debug!(
        "[{reaction_name}] HttpReaction adaptive mode using batch channel capacity: {capacity} (max_batch_size: {})",
        runtime_adaptive.max_batch_size
    );

    // Spawn batcher task. It owns the rx and exits when batch_tx is dropped.
    let batcher_handle = {
        let reaction_name = reaction_name.clone();
        let client = client.clone();
        let handlebars = handlebars.clone();
        let config = config.clone();
        let runtime_adaptive = runtime_adaptive.clone();
        tokio::spawn(async move {
            let mut batcher = AdaptiveBatcher::new(batch_rx, runtime_adaptive);
            let mut total_batches = 0u64;
            let mut total_results = 0u64;

            info!("[{reaction_name}] HTTP adaptive batcher started");

            while let Some(batch) = batcher.next_batch().await {
                if batch.is_empty() {
                    continue;
                }

                let batch_size: usize = batch.iter().map(|(_, v)| v.len()).sum();
                total_results += batch_size as u64;
                total_batches += 1;

                debug!("[{reaction_name}] Processing adaptive batch of {batch_size} results");

                if let Err(e) =
                    deliver_batch(&client, &handlebars, &config, &reaction_name, batch).await
                {
                    error!("[{reaction_name}] Failed to deliver batch: {e}");
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

    // Main: pull from priority queue, forward to batcher
    loop {
        let query_result_arc = tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                debug!("[{reaction_name}] Received shutdown signal, exiting adaptive loop");
                break;
            }
            result = base.priority_queue.dequeue() => result,
        };
        let query_result = query_result_arc.as_ref();

        if !matches!(status_handle.get_status().await, ComponentStatus::Running) {
            break;
        }

        if query_result.results.is_empty() {
            continue;
        }

        if batch_tx
            .send((query_result.query_id.clone(), query_result.results.clone()))
            .await
            .is_err()
        {
            error!("[{reaction_name}] Failed to send to batch channel — batcher exited");
            break;
        }
    }

    drop(batch_tx);
    let _ = tokio::time::timeout(Duration::from_secs(5), batcher_handle).await;

    info!("[{reaction_name}] HTTP adaptive loop stopped");
    status_handle
        .set_status(
            ComponentStatus::Stopped,
            Some("HTTP reaction processing task stopped".to_string()),
        )
        .await;
}

/// Either POST the whole batch to the configured `batch_endpoint`, or
/// fan the batch out and call [`process_result`] for each diff.
async fn deliver_batch(
    client: &Client,
    handlebars: &Handlebars<'static>,
    config: &HttpReactionConfig,
    reaction_name: &str,
    batch: Vec<(String, Vec<ResultDiff>)>,
) -> anyhow::Result<()> {
    // Coalesce by query_id
    let mut by_query: HashMap<String, Vec<ResultDiff>> = HashMap::new();
    for (qid, results) in batch {
        by_query.entry(qid).or_default().extend(results);
    }

    // If a batch_endpoint is configured and any query has multiple results,
    // use the coalesced batch POST.
    if let Some(batch_endpoint) = config.batch_endpoint.as_ref() {
        if by_query.values().any(|v| v.len() > 1) {
            return send_coalesced_batch(
                client,
                &config.base_url,
                batch_endpoint,
                &config.token,
                by_query,
                reaction_name,
            )
            .await;
        }
    }

    // Otherwise, fan out per-result through the standard rendering path.
    for (query_id, results) in by_query {
        for result in results {
            let (op, op_str): (OperationType, &str) = match &result {
                ResultDiff::Add { .. } => (OperationType::Add, "ADD"),
                ResultDiff::Delete { .. } => (OperationType::Delete, "DELETE"),
                ResultDiff::Update { .. } | ResultDiff::Aggregation { .. } => {
                    (OperationType::Update, "UPDATE")
                }
                ResultDiff::Noop => continue,
            };

            let spec_owned: HttpCallSpec;
            let spec: &HttpCallSpec = match config.get_template_spec(&query_id, op) {
                Some(s) => s,
                None => {
                    spec_owned = synthesized_default_spec(&query_id);
                    &spec_owned
                }
            };

            let data = match &result {
                ResultDiff::Add { data, .. } | ResultDiff::Delete { data, .. } => data.clone(),
                _ => {
                    serde_json::to_value(&result).expect("ResultDiff serialization should succeed")
                }
            };

            if let Err(e) = process_result(
                client,
                handlebars,
                &config.base_url,
                &config.token,
                spec,
                op_str,
                &data,
                &query_id,
                reaction_name,
            )
            .await
            {
                error!("[{reaction_name}] Failed to process result in batch: {e}");
            }
        }
    }

    Ok(())
}
