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

//! Standard per-result delivery loop.

use handlebars::Handlebars;
use log::{debug, error, info, warn};
use reqwest::Client;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::reactions::common::base::ReactionBase;
use drasi_lib::ReactionRecoveryPolicy;

use crate::checkpoint::{CheckpointState, FailureAction};
use crate::config::{HttpReactionConfig, TemplateRouting};
use crate::output::DefaultChangeNotification;
use crate::process::{post_default_notification, process_result};

/// Run the standard (per-result) processing loop. Blocks until the
/// shutdown signal fires or the queue is closed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_standard_loop(
    reaction_name: String,
    base: ReactionBase,
    config: HttpReactionConfig,
    client: Client,
    handlebars: Handlebars<'static>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    mut checkpoints: CheckpointState,
    policy: ReactionRecoveryPolicy,
) {
    let status_handle = base.status_handle();

    loop {
        let query_result_arc = tokio::select! {
            biased;
            _ = &mut shutdown_rx => {
                debug!("[{reaction_name}] Received shutdown signal, exiting standard loop");
                break;
            }
            result = base.priority_queue.dequeue() => result,
        };
        let query_result = query_result_arc.as_ref();

        if query_result.results.is_empty() {
            continue;
        }

        let query_name = &query_result.query_id;
        let seq = query_result.sequence;

        debug!(
            "[{reaction_name}] Processing {} results from query '{query_name}'",
            query_result.results.len()
        );

        let mut delivery_failed = false;
        for result in &query_result.results {
            let notification = match DefaultChangeNotification::from_diff(query_result, result) {
                Some(n) => n,
                // Noop variant: no notification, drop silently.
                None => continue,
            };

            let outcome = match config.get_template_spec(query_name, notification.operation_type())
            {
                Some(spec) => {
                    process_result(
                        &client,
                        &handlebars,
                        &config.base_url,
                        &config.token,
                        spec,
                        &notification,
                        query_name,
                        &reaction_name,
                    )
                    .await
                }
                None => {
                    post_default_notification(
                        &client,
                        &config.base_url,
                        &config.token,
                        &notification,
                        query_name,
                        &reaction_name,
                    )
                    .await
                }
            };

            if let Err(e) = outcome {
                error!("[{reaction_name}] Failed to process result: {e}");
                delivery_failed = true;
                // Under fail-stop, don't keep emitting the rest of this result.
                if FailureAction::from_policy(policy) == FailureAction::Stop {
                    break;
                }
            }
        }

        if delivery_failed {
            match FailureAction::from_policy(policy) {
                FailureAction::Stop => {
                    error!(
                        "[{reaction_name}] Delivery failed for query '{query_name}' (seq {seq}); \
                         stopping per Strict recovery policy — the result replays from the outbox on restart"
                    );
                    status_handle
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!(
                                "HTTP delivery failed for query '{query_name}' (seq {seq}); \
                                 stopped per recovery policy"
                            )),
                        )
                        .await;
                    return;
                }
                FailureAction::SkipAndContinue => {
                    warn!(
                        "[{reaction_name}] Delivery failed for query '{query_name}' (seq {seq}); \
                         skipping and advancing checkpoint per AutoSkipGap recovery policy"
                    );
                    if checkpoint_advance(&mut checkpoints, &base, &reaction_name, query_name, seq)
                        .await
                        .is_err()
                    {
                        // AutoSkipGap: a checkpoint-write failure is non-fatal.
                    }
                    continue;
                }
            }
        }

        // All diffs delivered: advance the checkpoint after the side effect.
        if checkpoint_advance(&mut checkpoints, &base, &reaction_name, query_name, seq)
            .await
            .is_err()
            && FailureAction::from_policy(policy) == FailureAction::Stop
        {
            status_handle
                .set_status(
                    ComponentStatus::Error,
                    Some(format!(
                        "HTTP checkpoint write failed for query '{query_name}' (seq {seq}); \
                         stopped per recovery policy"
                    )),
                )
                .await;
            return;
        }
    }

    info!("[{reaction_name}] HTTP standard loop stopped");
    status_handle
        .set_status(
            ComponentStatus::Stopped,
            Some("HTTP reaction processing task stopped".to_string()),
        )
        .await;
}

/// Advance the per-query checkpoint, logging (but not propagating) the error so
/// the caller can decide whether to stop based on the recovery policy.
async fn checkpoint_advance(
    checkpoints: &mut CheckpointState,
    base: &ReactionBase,
    reaction_name: &str,
    query_id: &str,
    sequence: u64,
) -> anyhow::Result<()> {
    if let Err(e) = checkpoints.advance(base, query_id, sequence).await {
        error!(
            "[{reaction_name}] Failed to write checkpoint for query '{query_id}' (seq {sequence}): {e}"
        );
        return Err(e);
    }
    Ok(())
}
