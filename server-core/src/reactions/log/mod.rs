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

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, trace, warn};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, QueryResultReceiver,
};
use crate::config::ReactionConfig;
use crate::utils::log_component_start;

use crate::reactions::Reaction;

pub struct LogReaction {
    config: ReactionConfig,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: ComponentEventSender,
    log_level: String,
}

impl LogReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        let log_level = config
            .properties
            .get("log_level")
            .and_then(|v| v.as_str())
            .unwrap_or("info")
            .to_string();

        Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
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
        match self.log_level.as_str() {
            "trace" => trace!("[{}] {}", self.config.id, message),
            "debug" => debug!("[{}] {}", self.config.id, message),
            "info" => info!("[{}] {}", self.config.id, message),
            "warn" => warn!("[{}] {}", self.config.id, message),
            "error" => error!("[{}] {}", self.config.id, message),
            _ => info!("[{}] {}", self.config.id, message),
        }
    }
}

#[async_trait]
impl Reaction for LogReaction {
    async fn start(&self, mut result_rx: QueryResultReceiver) -> Result<()> {
        log_component_start("Reaction", &self.config.id);

        *self.status.write().await = ComponentStatus::Starting;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting log reaction".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        *self.status.write().await = ComponentStatus::Running;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: Some("Log reaction started".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        self.log_result(&format!(
            "Started - receiving results from queries: {:?}",
            self.config.queries
        ));

        // Spawn a task to process results asynchronously
        let reaction_name = self.config.id.clone();
        let status_clone = self.status.clone();
        let log_level = self.log_level.clone();
        let config_name = self.config.id.clone();

        tokio::spawn(async move {
            let log_fn = |message: &str| match log_level.as_str() {
                "trace" => trace!("[{}] {}", config_name, message),
                "debug" => debug!("[{}] {}", config_name, message),
                "info" => info!("[{}] {}", config_name, message),
                "warn" => warn!("[{}] {}", config_name, message),
                "error" => error!("[{}] {}", config_name, message),
                _ => info!("[{}] {}", config_name, message),
            };

            while let Some(mut query_result) = result_rx.recv().await {
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
                        (profiling.source_send_ns, profiling.reaction_complete_ns) {
                        let total_latency_ns = reaction_complete - source_send;
                        debug!(
                            "[{}] End-to-end latency: {:.2}ms",
                            reaction_name,
                            total_latency_ns as f64 / 1_000_000.0
                        );
                    }
                }
            }

            info!(
                "[{}] Result channel closed, stopping reaction",
                reaction_name
            );
            *status_clone.write().await = ComponentStatus::Stopped;
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping log reaction: {}", self.config.id);
        *self.status.write().await = ComponentStatus::Stopped;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: Some("Log reaction stopped".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &ReactionConfig {
        &self.config
    }
}
