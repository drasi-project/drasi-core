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

use super::config::LogReactionConfig;
use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::sync::Arc;

use drasi_lib::channels::{ComponentEventSender, ComponentStatus};
use drasi_lib::config::common::LogLevel;
use drasi_lib::plugin_core::{QuerySubscriber, Reaction};
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::utils::log_component_start;

pub struct LogReaction {
    base: ReactionBase,
    config: LogReactionConfig,
}

impl LogReaction {
    /// Create a new log reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: LogReactionConfig) -> Self {
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    /// Create a new log reaction with custom priority queue capacity
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: LogReactionConfig,
        priority_queue_capacity: usize,
    ) -> Self {
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    fn format_result_static(result: &serde_json::Value, result_type: &str) -> String {
        if let Some(obj) = result.as_object() {
            let fields: Vec<String> = obj
                .iter()
                .map(|(k, v)| format!("{}: {}", k, Self::format_value_static(v)))
                .collect();
            format!("[{}] {}", result_type.to_uppercase(), fields.join(", "))
        } else {
            format!("[{}] {}", result_type.to_uppercase(), result)
        }
    }

    fn format_value_static(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Null => "null".to_string(),
            _ => value.to_string(),
        }
    }

    fn log_result(&self, message: &str) {
        match self.config.log_level {
            LogLevel::Trace => trace!("[{}] {}", self.base.id, message),
            LogLevel::Debug => debug!("[{}] {}", self.base.id, message),
            LogLevel::Info => info!("[{}] {}", self.base.id, message),
            LogLevel::Warn => warn!("[{}] {}", self.base.id, message),
            LogLevel::Error => error!("[{}] {}", self.base.id, message),
        }
    }
}

#[async_trait]
impl Reaction for LogReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "log"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "log_level".to_string(),
            serde_json::to_value(&self.config.log_level).unwrap_or(serde_json::Value::Null),
        );
        props
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    async fn start(
        &self,
        query_subscriber: Arc<dyn QuerySubscriber>,
    ) -> Result<()> {
        log_component_start("Reaction", &self.base.id);

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting log reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        self.base.subscribe_to_queries(query_subscriber).await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Log reaction started".to_string()),
            )
            .await?;

        self.log_result(&format!(
            "Started - receiving results from queries: {:?}",
            self.base.queries
        ));

        // Spawn processing task to dequeue and process results in timestamp order
        let priority_queue = self.base.priority_queue.clone();
        let reaction_name = self.base.id.clone();
        let log_level = self.config.log_level;
        let config_name = self.base.id.clone();

        let processing_task = tokio::spawn(async move {
            let log_fn = |message: &str| match log_level {
                LogLevel::Trace => trace!("[{}] {}", config_name, message),
                LogLevel::Debug => debug!("[{}] {}", config_name, message),
                LogLevel::Info => info!("[{}] {}", config_name, message),
                LogLevel::Warn => warn!("[{}] {}", config_name, message),
                LogLevel::Error => error!("[{}] {}", config_name, message),
            };

            loop {
                // Dequeue next result in timestamp order (blocking)
                let query_result_arc = priority_queue.dequeue().await;

                // Get mutable access to the result for profiling
                // Note: We need to clone and modify since Arc doesn't allow mutation
                let mut query_result = (*query_result_arc).clone();

                // Capture reaction_receive_ns timestamp
                if let Some(ref mut profiling) = query_result.profiling {
                    profiling.reaction_receive_ns = Some(drasi_lib::profiling::timestamp_ns());
                }

                if query_result.results.is_empty() {
                    debug!("[{}] Received empty result set from query", reaction_name);
                    continue;
                }

                log_fn(&format!(
                    "Query '{}' results ({} items):",
                    query_result.query_id,
                    query_result.results.len()
                ));

                for result in &query_result.results {
                    if let Some(result_type) = result.get("type").and_then(|v| v.as_str()) {
                        match result_type {
                            "add" | "remove" => {
                                if let Some(data) = result.get("data") {
                                    let formatted = Self::format_result_static(data, result_type);
                                    log_fn(&formatted);
                                }
                            }
                            "update" => {
                                if let (Some(before), Some(after)) =
                                    (result.get("before"), result.get("after"))
                                {
                                    let before_formatted =
                                        Self::format_result_static(before, "update_before");
                                    let after_formatted =
                                        Self::format_result_static(after, "update_after");
                                    log_fn(&format!(
                                        "[UPDATE] {} -> {}",
                                        before_formatted.trim_start_matches("[UPDATE_BEFORE] "),
                                        after_formatted.trim_start_matches("[UPDATE_AFTER] ")
                                    ));
                                }
                            }
                            _ => {
                                log_fn(&format!("[{}] {}", result_type.to_uppercase(), result));
                            }
                        }
                    }
                }

                // Capture reaction_complete_ns timestamp
                if let Some(ref mut profiling) = query_result.profiling {
                    profiling.reaction_complete_ns = Some(drasi_lib::profiling::timestamp_ns());

                    // Log profiling summary if available
                    if let (Some(source_send), Some(reaction_complete)) =
                        (profiling.source_send_ns, profiling.reaction_complete_ns)
                    {
                        let total_latency_ns = reaction_complete - source_send;
                        debug!(
                            "[{}] End-to-end latency: {:.2}ms",
                            reaction_name,
                            total_latency_ns as f64 / 1_000_000.0
                        );
                    }
                }
            }
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_task).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Use ReactionBase common stop functionality
        self.base.stop_common().await?;

        // Transition to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Log reaction stopped".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}

#[cfg(test)]
mod tests;
