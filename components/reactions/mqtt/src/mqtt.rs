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

use super::MqttReactionBuilder;
use crate::config::{MqttCallSpec, MqttQueryConfig, MqttReactionConfig, RetainPolicy};

use anyhow::Result;
pub struct MqttReaction {
    base: ReactionBase,
    config: MqttReactionConfig,
}
use drasi_core::evaluation::variable_value::de;
use drasi_lib::{
    channels::{ComponentStatus, ResultDiff},
    managers::log_component_start,
    Reaction
};
use log::{debug, info, warn};

use async_trait::async_trait;
use drasi_lib::reactions::{ ReactionBase, ReactionBaseParams};
use std::collections::HashMap;

impl MqttReaction {
    // Create a builder for MqttReaction
    pub fn builder(id: impl Into<String>) -> MqttReactionBuilder {
        MqttReactionBuilder::new(id)
    }

    /// Create a new MQTT reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: MqttReactionConfig) -> Self {
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    /// Create a new MQTT reaction with custom priority queue capacity
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: MqttReactionConfig,
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

    /// Create from builder (internal method)
    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: MqttReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }
}

#[async_trait]
impl Reaction for MqttReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "mqtt"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "base_topic".to_string(),
            serde_json::Value::String(self.config.base_topic.clone()),
        );
        props.insert(
            "timeout_ms".to_string(),
            serde_json::Value::Number(self.config.timeout_ms.into()),
        );
        props
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("MQTT Reaction", &self.base.id);

        info!(
            "[{}] MQTT reaction started - sending to base topic: {}",
            self.base.id, self.config.base_topic
        );

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting MQTT reaction".to_string()),
            )
            .await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("MQTT reaction started".to_string()),
            )
            .await?;

        // Create shutdown channel for graceful termination
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // Spawn the main processing task
        let reaction_name = self.base.id.clone();
        let status = self.base.status.clone();
        let query_configs = self.config.routes.clone();
        let base_topic = self.config.base_topic.clone();
        let timeout_ms = self.config.timeout_ms;
        let priority_queue: drasi_lib::channels::PriorityQueue<drasi_lib::channels::QueryResult> =
            self.base.priority_queue.clone();

        let processing_task_handle = tokio::spawn(async move {
            loop {
                let query_result_arc = tokio::select! {
                    biased; // give priority to shutdown

                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                        break;
                    },

                    result = priority_queue.dequeue() => result,
                };

                if !matches!(*status.read().await, ComponentStatus::Running) {
                    break;
                }

                let query_result_ref = query_result_arc.as_ref();
                if query_result_ref.results.is_empty() {
                    debug!("[{reaction_name}] Recieved empty result set from query");
                    continue;
                }

                let query_name = &query_result_ref.query_id;

                // Check if we have a route configured for this query
                let route = query_configs.get(query_name).or_else(|| {
                    query_name
                        .split('.')
                        .next_back()
                        .and_then(|name| query_configs.get(name))
                });

                let query_config = match route {
                    Some(config) => config.clone(),
                    None => {
                        debug!(
                            "[{reaction_name}] No configuration for query '{query_name}', using default"
                        );
                        default_config_for_query(query_name, &base_topic)
                    }
                };

                debug!(
                    "[{}] Processing {} results from query '{}'",
                    reaction_name,
                    query_result_ref.results.len(),
                    query_name
                );

                // Process each result
                for result in &query_result_ref.results {
                    println!("Processing result: {result:?}");
                }
            }

            info!("[{reaction_name}] MQTT reaction stopped");
            *status.write().await = ComponentStatus::Stopped;
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_task_handle).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Use ReactionBase common stop functionality
        self.base.stop_common().await?;

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Transition to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("MQTT reaction stopped successfully".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(
        &self,
        result: drasi_lib::channels::QueryResult,
    ) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }
}

// ==================================
//  helper methods
// ==================================
#[inline]
fn default_config_for_query(query_name: &str, base_topic: &str) -> MqttQueryConfig {
    MqttQueryConfig {
        added: Some(MqttCallSpec {
            topic: format!("/{base_topic}"),
            body: String::new(),
            retain: RetainPolicy::default(),
            headers: HashMap::new(),
        }),
        updated: Some(MqttCallSpec {
            topic: format!("/{base_topic}"),
            body: String::new(),
            retain: RetainPolicy::default(),
            headers: HashMap::new(),
        }),
        deleted: Some(MqttCallSpec {
            topic: format!("/{base_topic}"),
            body: String::new(),
            retain: RetainPolicy::default(),
            headers: HashMap::new(),
        }),
    }
}
