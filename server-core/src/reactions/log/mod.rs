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

pub mod config;
pub use config::LogReactionConfig;

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, trace, warn};
use std::sync::Arc;

use crate::channels::{ComponentEventSender, ComponentStatus};
use crate::config::common::LogLevel;
use crate::config::ReactionConfig;
use crate::reactions::common::base::ReactionBase;
use crate::reactions::Reaction;
use crate::utils::log_component_start;

pub struct LogReaction {
    base: ReactionBase,
    log_level: LogLevel,
}

impl LogReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        let log_level = match &config.config {
            crate::config::ReactionSpecificConfig::Log(log_config) => log_config.log_level,
            _ => LogLevel::Info,
        };

        Self {
            base: ReactionBase::new(config, event_tx),
            log_level,
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
        match self.log_level {
            LogLevel::Trace => trace!("[{}] {}", self.base.config.id, message),
            LogLevel::Debug => debug!("[{}] {}", self.base.config.id, message),
            LogLevel::Info => info!("[{}] {}", self.base.config.id, message),
            LogLevel::Warn => warn!("[{}] {}", self.base.config.id, message),
            LogLevel::Error => error!("[{}] {}", self.base.config.id, message),
        }
    }
}

#[async_trait]
impl Reaction for LogReaction {
    async fn start(
        &self,
        query_subscriber: Arc<dyn crate::reactions::common::base::QuerySubscriber>,
    ) -> Result<()> {
        log_component_start("Reaction", &self.base.config.id);

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
            self.base.config.queries
        ));

        // Spawn processing task to dequeue and process results in timestamp order
        let priority_queue = self.base.priority_queue.clone();
        let reaction_name = self.base.config.id.clone();
        let log_level = self.log_level;
        let config_name = self.base.config.id.clone();

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
                    profiling.reaction_receive_ns = Some(crate::profiling::timestamp_ns());
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
                    profiling.reaction_complete_ns = Some(crate::profiling::timestamp_ns());

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

    fn get_config(&self) -> &ReactionConfig {
        &self.base.config
    }
}

#[cfg(test)]
mod tests;
