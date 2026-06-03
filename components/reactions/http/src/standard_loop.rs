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

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::reactions::common::base::ReactionBase;

use crate::config::{
    synthesized_default_spec, HttpCallSpec, HttpReactionConfig, OperationType, TemplateRouting,
};
use crate::process::process_result;

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

        if !matches!(status_handle.get_status().await, ComponentStatus::Running) {
            break;
        }

        if query_result.results.is_empty() {
            continue;
        }

        let query_name = &query_result.query_id;
        debug!(
            "[{reaction_name}] Processing {} results from query '{query_name}'",
            query_result.results.len()
        );

        for result in &query_result.results {
            let (op, op_str): (OperationType, &str) = match result {
                ResultDiff::Add { .. } => (OperationType::Add, "ADD"),
                ResultDiff::Delete { .. } => (OperationType::Delete, "DELETE"),
                ResultDiff::Update { .. } | ResultDiff::Aggregation { .. } => {
                    (OperationType::Update, "UPDATE")
                }
                ResultDiff::Noop => continue,
            };

            let spec_owned: HttpCallSpec;
            let spec: &HttpCallSpec = match config.get_template_spec_with_suffix(query_name, op) {
                Some(s) => s,
                None => {
                    spec_owned = synthesized_default_spec(query_name);
                    &spec_owned
                }
            };

            let data = match result {
                ResultDiff::Add { data, .. } | ResultDiff::Delete { data, .. } => data.clone(),
                _ => serde_json::to_value(result).expect("ResultDiff serialization should succeed"),
            };

            if let Err(e) = process_result(
                &client,
                &handlebars,
                &config.base_url,
                &config.token,
                spec,
                op_str,
                &data,
                query_name,
                &reaction_name,
            )
            .await
            {
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
