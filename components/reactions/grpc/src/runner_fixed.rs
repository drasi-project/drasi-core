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

//! Fixed-size batching runner.
//!
//! Accumulates `ProtoQueryResultItem`s and flushes when either the
//! configured batch size or flush timeout is reached. This is the default
//! batching mode and matches the historical behavior of `drasi-reaction-grpc`
//! prior to the unification with `grpc-adaptive`.

use std::collections::HashMap;
use std::time::Duration;

use log::{debug, error, info, trace, warn};
use tokio::sync::oneshot;
use tonic::transport::Channel;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::reactions::common::base::ReactionBase;

use crate::config::GrpcReactionConfig;
use crate::connection::{create_client_with_retry, ConnectionState};
use crate::proto::ReactionServiceClient;
use crate::send::send_batch_with_retry;
use crate::templates::{build_proto_item, TemplateEngine};

pub(crate) struct FixedRunnerParams {
    pub reaction_name: String,
    pub batch_size: usize,
    pub batch_flush_timeout_ms: u64,
    pub base: ReactionBase,
    pub config: GrpcReactionConfig,
    pub shutdown_rx: oneshot::Receiver<()>,
}

pub(crate) async fn run(params: FixedRunnerParams) {
    let FixedRunnerParams {
        reaction_name,
        batch_size,
        batch_flush_timeout_ms,
        base,
        config,
        mut shutdown_rx,
    } = params;

    let endpoint = config.endpoint.clone();
    let metadata = config.metadata.clone();
    let max_retries = config.max_retries;
    let timeout_ms = config.timeout_ms;
    let connection_retry_attempts = config.connection_retry_attempts;
    let initial_connection_timeout_ms = config.initial_connection_timeout_ms;
    let template_engine = config
        .output_templates
        .as_ref()
        .filter(|t| t.has_renderable_templates())
        .map(|_| TemplateEngine::new());

    let status_handle = base.status_handle();
    let priority_queue = base.priority_queue.clone();

    info!("gRPC reaction starting for endpoint: {endpoint} (lazy connection)");

    let mut client: Option<ReactionServiceClient<Channel>> = None;
    let mut connection_state = ConnectionState::Disconnected;

    let mut consecutive_failures = 0u32;
    let mut last_connection_attempt = std::time::Instant::now();
    let base_backoff = Duration::from_millis(500);
    let max_backoff = Duration::from_secs(30);

    let mut batch = Vec::new();
    let mut last_query_id = String::new();
    let flush_timeout = Duration::from_millis(batch_flush_timeout_ms);

    let mut flush_timer = tokio::time::interval(flush_timeout);
    flush_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        let query_result = tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                break;
            }
            _ = flush_timer.tick() => {
                // Flush the in-flight batch when the flush timeout elapses so
                // low-throughput queries aren't held indefinitely waiting to
                // reach `batch_size`.
                if !batch.is_empty() && !last_query_id.is_empty() {
                    ensure_client(
                        &mut client,
                        &mut connection_state,
                        &endpoint,
                        initial_connection_timeout_ms,
                        connection_retry_attempts,
                        &mut consecutive_failures,
                        &mut last_connection_attempt,
                        base_backoff,
                        max_backoff,
                    )
                    .await;

                    if client.is_some() {
                        let sent = send_with_swap(
                            &mut client,
                            &batch,
                            &last_query_id,
                            &metadata,
                            max_retries,
                            &endpoint,
                            timeout_ms,
                            &reaction_name,
                            &mut connection_state,
                            &mut consecutive_failures,
                            &mut last_connection_attempt,
                        )
                        .await;
                        if sent {
                            batch.clear();
                        }
                    }
                }
                continue;
            }
            result = priority_queue.dequeue() => result,
        };

        debug!(
            "Dequeued query result with {} items for processing",
            query_result.results.len()
        );

        if !matches!(status_handle.get_status().await, ComponentStatus::Running) {
            info!(
                "[{}] Reaction status changed to non-running, exiting main loop (batch has {} items)",
                reaction_name,
                batch.len()
            );
            break;
        }

        if query_result.results.is_empty() {
            debug!("[{reaction_name}] Received empty result set from query");
            continue;
        }

        let query_id = &query_result.query_id;
        trace!("Processing results for query_id: {query_id}");

        if !last_query_id.is_empty() && last_query_id != query_result.query_id && !batch.is_empty()
        {
            ensure_client(
                &mut client,
                &mut connection_state,
                &endpoint,
                initial_connection_timeout_ms,
                connection_retry_attempts,
                &mut consecutive_failures,
                &mut last_connection_attempt,
                base_backoff,
                max_backoff,
            )
            .await;

            if client.is_some() {
                let sent = send_with_swap(
                    &mut client,
                    &batch,
                    &last_query_id,
                    &metadata,
                    max_retries,
                    &endpoint,
                    timeout_ms,
                    &reaction_name,
                    &mut connection_state,
                    &mut consecutive_failures,
                    &mut last_connection_attempt,
                )
                .await;
                if sent {
                    batch.clear();
                }
            }
        }

        last_query_id = query_id.clone();

        for result in &query_result.results {
            let proto_item = build_proto_item(&config, template_engine.as_ref(), query_id, result);
            batch.push(proto_item);

            if batch.len() >= batch_size {
                ensure_client(
                    &mut client,
                    &mut connection_state,
                    &endpoint,
                    initial_connection_timeout_ms,
                    connection_retry_attempts,
                    &mut consecutive_failures,
                    &mut last_connection_attempt,
                    base_backoff,
                    max_backoff,
                )
                .await;

                if client.is_some() {
                    let sent = send_with_swap(
                        &mut client,
                        &batch,
                        query_id,
                        &metadata,
                        max_retries,
                        &endpoint,
                        timeout_ms,
                        &reaction_name,
                        &mut connection_state,
                        &mut consecutive_failures,
                        &mut last_connection_attempt,
                    )
                    .await;
                    if sent {
                        batch.clear();
                    }
                }
            }
        }
    }

    // Final flush
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
                Ok(c) => client = Some(c),
                Err(e) => {
                    error!("[{reaction_name}] Failed to create client for final batch: {e}");
                    finish(&reaction_name, &status_handle).await;
                    return;
                }
            }
        }
        let _ = send_with_swap(
            &mut client,
            &batch,
            &last_query_id,
            &metadata,
            max_retries,
            &endpoint,
            timeout_ms,
            &reaction_name,
            &mut connection_state,
            &mut consecutive_failures,
            &mut last_connection_attempt,
        )
        .await;
    }

    finish(&reaction_name, &status_handle).await;
}

async fn finish(
    reaction_name: &str,
    status_handle: &drasi_lib::component_graph::ComponentStatusHandle,
) {
    info!("[{reaction_name}] gRPC reaction processing task stopped");
    status_handle
        .set_status(
            ComponentStatus::Stopped,
            Some("gRPC reaction processing task stopped".to_string()),
        )
        .await;
}

#[allow(clippy::too_many_arguments)]
async fn ensure_client(
    client: &mut Option<ReactionServiceClient<Channel>>,
    connection_state: &mut ConnectionState,
    endpoint: &str,
    initial_connection_timeout_ms: u64,
    connection_retry_attempts: u32,
    consecutive_failures: &mut u32,
    last_connection_attempt: &mut std::time::Instant,
    base_backoff: Duration,
    max_backoff: Duration,
) {
    if client.is_some() && matches!(connection_state, ConnectionState::Connected) {
        return;
    }
    let time_since_last_attempt = last_connection_attempt.elapsed();
    let backoff_duration = base_backoff * 2u32.pow((*consecutive_failures).min(10));
    let backoff_duration = backoff_duration.min(max_backoff);

    if time_since_last_attempt < backoff_duration && *connection_state == ConnectionState::Failed {
        let wait_time = backoff_duration - time_since_last_attempt;
        warn!(
            "State: {} - Waiting {:.2}s before retrying (failures: {})",
            connection_state,
            wait_time.as_secs_f64(),
            *consecutive_failures
        );
        return;
    }

    *last_connection_attempt = std::time::Instant::now();

    match create_client_with_retry(
        endpoint,
        initial_connection_timeout_ms,
        connection_retry_attempts,
    )
    .await
    {
        Ok(c) => {
            *connection_state = ConnectionState::Connected;
            *consecutive_failures = 0;
            *client = Some(c);
        }
        Err(e) => {
            *connection_state = ConnectionState::Failed;
            *consecutive_failures += 1;
            error!(
                "State transition: Connecting -> Failed (attempt {}): {e}",
                *consecutive_failures
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_with_swap(
    client: &mut Option<ReactionServiceClient<Channel>>,
    batch: &[crate::proto::ProtoQueryResultItem],
    query_id: &str,
    metadata: &HashMap<String, String>,
    max_retries: u32,
    endpoint: &str,
    timeout_ms: u64,
    reaction_name: &str,
    connection_state: &mut ConnectionState,
    consecutive_failures: &mut u32,
    last_connection_attempt: &mut std::time::Instant,
) -> bool {
    let mut retry_swaps = 0u32;
    loop {
        let Some(c) = client.as_mut() else {
            return false;
        };
        match send_batch_with_retry(
            c,
            batch.to_vec(),
            query_id,
            metadata,
            max_retries,
            endpoint,
            timeout_ms,
        )
        .await
        {
            Ok((needs_new_client, new_client)) => {
                if needs_new_client {
                    if let Some(nc) = new_client {
                        *connection_state = ConnectionState::Connected;
                        *client = Some(nc);
                        *consecutive_failures = 0;
                        retry_swaps += 1;
                        if retry_swaps > 2 {
                            return false;
                        }
                        continue;
                    } else {
                        *connection_state = ConnectionState::Reconnecting;
                        *client = None;
                        *consecutive_failures += 1;
                        *last_connection_attempt = std::time::Instant::now();
                        return false;
                    }
                } else {
                    *consecutive_failures = 0;
                    return true;
                }
            }
            Err(e) => {
                error!("[{reaction_name}] Failed to send batch: {e}");
                return false;
            }
        }
    }
}
