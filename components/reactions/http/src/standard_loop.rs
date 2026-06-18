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
use log::{debug, error, info};
use reqwest::Client;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::reactions::common::base::ReactionBase;

use crate::config::{HttpReactionConfig, TemplateRouting};
use crate::output::DefaultChangeNotification;
use crate::process::{post_default_notification, process_result};

/// Run the standard (per-result) processing loop. Blocks until the
/// shutdown signal fires or the queue is closed.
pub(crate) async fn run_standard_loop(
    reaction_name: String,
    base: ReactionBase,
    config: HttpReactionConfig,
    client: Client,
    handlebars: Handlebars<'static>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
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
        debug!(
            "[{reaction_name}] Processing {} results from query '{query_name}'",
            query_result.results.len()
        );

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
            }
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
