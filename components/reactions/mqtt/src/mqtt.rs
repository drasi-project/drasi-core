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
use crate::config::{
    MqttAuthMode, MqttCallSpec, MqttQueryConfig, MqttReactionConfig, RetainPolicy,
};

use anyhow::Result;
pub struct MqttReaction {
    base: ReactionBase,
    config: MqttReactionConfig,
}
use drasi_core::evaluation::variable_value::de;
use drasi_lib::{
    channels::{ComponentStatus, ResultDiff},
    managers::log_component_start,
    Reaction,
};
use log::{debug, error, info, warn};

use async_trait::async_trait;
use drasi_lib::reactions::{ReactionBase, ReactionBaseParams};
use serde_json::{Map, Value};
use std::{collections::HashMap, os::unix::process, time::Duration};

use handlebars::{template, Handlebars};
use rumqttc::v5::{mqttbytes::QoS, AsyncClient, Event, Incoming, MqttOptions};
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

    #[allow(clippy::too_many_arguments)]
    async fn process_result(
        id: impl Into<String>,
        mqtt_client: &AsyncClient,
        handlebars: &Handlebars<'_>,
        result: &ResultDiff,
        query_name: &str,
        query_config: &MqttQueryConfig,
    ) -> Result<()> {
        match result {
            ResultDiff::Aggregation { .. } | ResultDiff::Noop => {
                debug!(
                    "[{}] Received aggregation or noop result, skipping MQTT call",
                    id.into()
                );
                return Ok(());
            }
            _ => {}
        }

        let mut type_str = "";
        let mut context: Map<String, Value> = Map::new();
        let mut data = match result {
            ResultDiff::Add { data } => {
                if let Some(spec) = query_config.added.as_ref() {
                    context.insert("after".to_string(), data.clone());
                    type_str = "ADD";
                    Some(data.clone())
                } else {
                    None
                }
            }
            ResultDiff::Update { .. } => {
                if let Some(spec) = query_config.updated.as_ref() {
                    let data_to_process = serde_json::to_value(result)
                        .expect("ResultDiff serialization should succeed");
                    let data = data_to_process.clone();
                    if let Some(obj) = data_to_process.as_object() {
                        if let Some(before) = obj.get("before") {
                            context.insert("before".to_string(), before.clone());
                        }
                        if let Some(after) = obj.get("after") {
                            context.insert("after".to_string(), after.clone());
                        }
                        if let Some(data_field) = obj.get("data") {
                            context.insert("data".to_string(), data_field.clone());
                        }
                    } else {
                        context.insert("after".to_string(), data_to_process);
                    }
                    type_str = "UPDATE";
                    Some(data)
                } else {
                    None
                }
            }
            ResultDiff::Delete { data } => {
                if let Some(spec) = query_config.deleted.as_ref() {
                    context.insert("before".to_string(), data.clone());
                    type_str = "DELETE";
                    Some(data.clone())
                } else {
                    None
                }
            }
            _ => {
                // This should not happen due to the early check.
                None
            }
        };

        context.insert(
            "query_name".to_string(),
            Value::String(query_name.to_string()),
        );

        let type_str = result_type(result);
        context.insert("operation".to_string(), Value::String(type_str.to_string()));

        let call_spec_result = match type_str {
            "ADD" => query_config.added.as_ref(),
            "UPDATE" => query_config.updated.as_ref(),
            "DELETE" => query_config.deleted.as_ref(),
            _ => None,
        };

        if let Some(call_spec) = call_spec_result {
            let body = if !call_spec.body.is_empty() {
                handlebars.render_template(&call_spec.body, &context)?
            } else {
                serde_json::to_string(&data)?
            };
            let topic = call_spec.topic.clone();
            let qos = match call_spec.qos {
                crate::config::QualityOfService::AtMostOnce => QoS::AtMostOnce,
                crate::config::QualityOfService::AtLeastOnce => QoS::AtLeastOnce,
                crate::config::QualityOfService::ExactlyOnce => QoS::ExactlyOnce,
            };

            let retain = match call_spec.retain {
                RetainPolicy::Retain => true,
                RetainPolicy::NoRetain => false,
            };

            match mqtt_client.publish(topic, qos, retain, body).await {
                Ok(_) => {
                    debug!(
                        "[{}] Published MQTT message for {} operation on query '{}'",
                        id.into(),
                        type_str,
                        query_name
                    );
                }
                Err(e) => {
                    error!(
                        "[{}] Failed to publish MQTT message for {} operation on query '{}': {}",
                        id.into(),
                        type_str,
                        query_name,
                        e
                    );
                }
            }
        }
        Ok(())
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
            "default_topic".to_string(),
            serde_json::Value::String(self.config.default_topic.clone()),
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
            "[{}] MQTT reaction started - sending to default topic: {}",
            self.base.id, self.config.default_topic
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
        let host = self.config.host.clone();
        let port = self.config.port;
        let status = self.base.status.clone();
        let query_configs = self.config.routes.clone();
        let default_topic = self.config.default_topic.clone();
        let timeout_ms = self.config.timeout_ms;
        let priority_queue: drasi_lib::channels::PriorityQueue<drasi_lib::channels::QueryResult> =
            self.base.priority_queue.clone();
        let capacity = self.config.capacity;
        let auth_mode = self.config.auth_mode.clone();

        let processing_task_handle = tokio::spawn(async move {
            // Initialize MQTT client options
            let mqtt_id = reaction_name.clone();
            let reaction_name_eventloop = reaction_name.clone();
            let mut options = MqttOptions::new(mqtt_id, host, port);
            options.set_keep_alive(Duration::from_secs(timeout_ms));

            // Set up Handlebars with json helper
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

            if let MqttAuthMode::UsernamePassword { username, password } = auth_mode {
                options.set_credentials(username, password);
            }

            // Create MQTT client
            let (mqtt_client, mut eventloop) = AsyncClient::new(options, capacity);

            // Task to handle MQTT connection events
            let (mut event_loop_shutdown_tx, mut event_loop_shutdown_rx) =
                tokio::sync::mpsc::channel(1);
            let event_loop_handler = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = event_loop_shutdown_rx.recv() => {
                            info!("[{reaction_name_eventloop}] Received MQTT event loop shutdown signal, exiting event loop");
                            break;
                        },
                        event = eventloop.poll() => {
                            match event {
                                Ok(_) => {},
                                Err(e) => {
                                    warn!("[{reaction_name_eventloop}] MQTT event loop error: {e}");
                                }
                            }
                        }
                    }
                }
            });

            loop {
                let query_result_arc = tokio::select! {
                    biased; // give priority to shutdown

                    _ = &mut shutdown_rx => {
                        info!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                        event_loop_shutdown_tx.send(()).await;
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
                    query_configs.get("*").or_else(|| {
                        query_name
                            .split('.')
                            .next_back()
                            .and_then(|name| query_configs.get(name))
                    })
                });

                let query_config = match route {
                    Some(config) => config.clone(),
                    None => {
                        debug!(
                            "[{reaction_name}] No configuration for query '{query_name}', using default"
                        );
                        default_config_for_query(&default_topic)
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
                    if let Err(e) = Self::process_result(
                        reaction_name.clone(),
                        &mqtt_client,
                        &handlebars,
                        result,
                        query_name,
                        &query_config,
                    )
                    .await
                    {
                        error!("[{reaction_name}] Failed to process result: {e}");
                    }
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
fn default_config_for_query(default_topic: &str) -> MqttQueryConfig {
    MqttQueryConfig {
        added: Some(MqttCallSpec {
            topic: format!("/{default_topic}"),
            retain: RetainPolicy::default(),
            body: String::new(),
            headers: HashMap::new(),
            qos: crate::config::QualityOfService::ExactlyOnce,
        }),
        updated: Some(MqttCallSpec {
            topic: format!("/{default_topic}"),
            retain: RetainPolicy::default(),
            body: String::new(),
            headers: HashMap::new(),
            qos: crate::config::QualityOfService::ExactlyOnce,
        }),
        deleted: Some(MqttCallSpec {
            topic: format!("/{default_topic}"),
            retain: RetainPolicy::default(),
            body: String::new(),
            headers: HashMap::new(),
            qos: crate::config::QualityOfService::ExactlyOnce,
        }),
    }
}

#[inline]
fn result_type(result_diff: &ResultDiff) -> &'static str {
    match result_diff {
        ResultDiff::Add { .. } => "ADD",
        ResultDiff::Update { .. } => "UPDATE",
        ResultDiff::Delete { .. } => "DELETE",
        _ => "",
    }
}
