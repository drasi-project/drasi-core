// Copyright 2025 The Drasi Authors.
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

use drasi_core::evaluation::functions::Date;
use drasi_lib::channels::{PriorityQueue, QueryResult, ResultDiff, priority_queue};
use drasi_lib::reactions::common::TemplateRouting;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use crate::metric::{ShellReactionInternalState, ShellReactionMetrices};
use crate::config::{ShellExtension, ShellReactionConfig};
use std::collections::HashMap;
use std::process;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio::sync::oneshot::Receiver;
use drasi_lib::reactions::common::OperationType;
use serde_json::{Map, json};
use handlebars::Handlebars;

use log::{info};

#[derive(Debug, Clone)]
struct CommandExecutionContext {
    stdin: bool,
    stdin_data: String,
    env: HashMap<String, String>,
    kill_on_drop: bool,
    timeout_s: u64,
    capture_limits: usize,
}

#[derive(Debug)]
pub struct ShellExecutor {
    reaction_id: String,
    metrices: Arc<ShellReactionMetrices>,
    internal_state: Arc<ShellReactionInternalState>,
    config: Arc<ShellReactionConfig>
}

impl ShellExecutor {
    pub fn new(reaction_id: String, config: ShellReactionConfig) -> Self {
        Self {
            reaction_id,
            metrices: Arc::new(ShellReactionMetrices::new()),
            internal_state: Arc::new(ShellReactionInternalState::new()),
            config: Arc::new(config),
        }
    }

    pub fn start_processing_loop(&self, priority_queue: PriorityQueue<QueryResult>, mut shutdown_rx: tokio::sync::oneshot::Receiver<()>) -> anyhow::Result<JoinHandle<()>> {
        let max_concurrent = self.config.max_concurrent;
        let config_arc = self.config.clone();

        // create handlebars
        let mut handlebars = Handlebars::new();
        handlebars.register_helper(
            "json",
            Box::new(
                |h: &handlebars::Helper,
                    _: &Handlebars,
                    _: &handlebars::Context,
                    _: &mut handlebars::RenderContext,
                    out: &mut dyn handlebars::Output|
                    -> handlebars::HelperResult {
                    if let Some(value) = h.param(0) {
                        let json_str = serde_json::to_string(&value.value())
                            .unwrap_or_else(|_| "null".to_string());
                        out.write(&json_str)?;
                    }
                    Ok(())
                },
            ),
        );

        // spawn a background task
        Ok(tokio::spawn(
            async move {
                let sem = Arc::new(Semaphore::new(max_concurrent));
                let cancel = CancellationToken::new();

                loop {
                    tokio::select! {
                        biased;

                        _ = &mut shutdown_rx => {
                            // Info LOG
                            cancel.cancel();
                            break;
                        }


                        result = priority_queue.dequeue() => {
                            let query_result = result.as_ref();

                            if(query_result.results.is_empty()){
                                // drops the control signals like `bootstrapStarted` or `bootstrapCompleted`
                                // DEBUG LOG
                                continue;
                            } else {
                                Self::process_results(cancel.clone(), query_result, config_arc.clone(), sem.clone(), &handlebars);
                            }
                        }
                    }
                }
            }
        ))
    }

    fn process_results(cancel: CancellationToken, query_result: &QueryResult, config: Arc<ShellReactionConfig>, sem: Arc<Semaphore>, handlebars: &Handlebars<'static>) {
        let query_id = query_result.query_id.clone();

        for result in &query_result.results {
            if matches!(result, ResultDiff::Noop) {
                // DEBUG LOG
                continue;
            }

            Self::process_single_result(cancel.clone(), query_id.clone(), result.clone(), config.clone(), sem.clone(), handlebars);
        }
    }

    fn process_single_result(
        cancel: CancellationToken,
        query_id: String,
        result: ResultDiff,
        config: Arc<ShellReactionConfig>,
        sem: Arc<Semaphore>,
        handlebars: &Handlebars<'static>
    ) -> anyhow::Result<()> {

        // get the operation, data val and result type
        let (operation, data_value, result_type) = match result {
            ResultDiff::Add { data } => (OperationType::Add, data, "ADD"),
            ResultDiff::Update { data, .. } => (OperationType::Update, data, "UPDATE"),
            ResultDiff::Delete { data } => (OperationType::Delete, data, "DELETE"),
            ResultDiff::Aggregation { before, after } => {
                let aggregation_data = json!({
                    "before": before,
                    "after": after,
                });
                (OperationType::Update, aggregation_data, "AGGREGATION")
            },
            ResultDiff::Noop => return Ok(()), // already handled above
        };

        // build the context map for template rendering
        let mut context = Map::new();
        match result_type {
            "ADD" => {
                context.insert("after".to_string(), data_value.clone());
            },
            "UPDATE" | "AGGREGATION" => {

                if let Some(obj) = data_value.as_object() {
                    if let Some(before) = obj.get("before") {
                        context.insert("before".to_string(), before.clone());
                    }
                    if let Some(after) = obj.get("after") {
                        context.insert("after".to_string(), after.clone());
                    }
                    if let Some(data) = obj.get("data") {
                        context.insert("data".to_string(), data.clone());
                    }
                } else {
                    context.insert("after".to_string(), data_value.clone());
                }

            },
            "DELETE" => {
                context.insert("before".to_string(), data_value.clone());
            },
            _ => {
                context.insert("data".to_string(), data_value.clone());
            }
        }

        // get the target template for query and operation
        let spec: Option<&drasi_lib::reactions::common::TemplateSpec<ShellExtension>> = config.get_template_spec(&query_id, operation);


        // create the env vars for the command and render the global env values.
        let mut command_env = HashMap::new();
        for (key, value) in &config.env {
            let rendered_value = handlebars.render_template(value, &context)?;
            command_env.insert(key.clone(), rendered_value);
        }

        if let Some(spec) = spec {

            // load and render the local env values if exist.
            for (key, value) in &spec.extension.envs {
                let rendered_value = handlebars.render_template(value, &context)?;
                command_env.insert(key.clone(), rendered_value);
            }

            // render the stdin data
            let rendered_stdin = handlebars.render_template(&spec.template, &context)?;
            println!("Rendered command for query '{}': {}", query_id, rendered_stdin);

        } else {
            // no body template found for the query, send json raw data
            println!("Sending raw data for query '{}': {}", query_id, data_value);

        }

        Ok(())

    }

    fn execute_command(){

    }
}