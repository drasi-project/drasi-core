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

use super::processor::Processor;
use super::MqttReactionBuilder;
use crate::adaptive_batcher::AdaptiveBatchConfig;
use crate::config::{
    MqttAuthMode, MqttQueryConfig, MqttReactionConfig, MqttTransportMode, RetainPolicy,
};
use anyhow::Result;
use drasi_core::evaluation::variable_value::de;
use drasi_lib::{
    channels::{ComponentStatus, ResultDiff},
    managers::log_component_start,
    Reaction,
};
use log::{debug, error, info, warn};
use std::{default, sync::Arc};

use async_trait::async_trait;
use drasi_lib::reactions::{ReactionBase, ReactionBaseParams};
use serde_json::{Map, Value};
use std::{collections::HashMap, os::unix::process, sync::RwLock, time::Duration};

use handlebars::{template, Handlebars};
use rumqttc::v5::{mqttbytes::QoS, AsyncClient, Event, Incoming, MqttOptions};

pub struct MqttReaction {
    base: ReactionBase,
    config: MqttReactionConfig,
    adaptive_config: AdaptiveBatchConfig,
}

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
        Self::create_internal(id.into(), queries, config, None, true)
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
        Self::create_internal(
            id.into(),
            queries,
            config,
            Some(priority_queue_capacity),
            true,
        )
    }

    /// Create from builder (internal method)
    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: MqttReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        Self::create_internal(id, queries, config, priority_queue_capacity, auto_start)
    }

    /// Internal Contructor
    fn create_internal(
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
        let adaptive_mqtt_config = match config.adaptive.clone() {
            Some(adaptive_config) => {
                // Adaptive batcher.
                AdaptiveBatchConfig {
                    min_batch_size: adaptive_config.adaptive_min_batch_size,
                    max_batch_size: adaptive_config.adaptive_max_batch_size,
                    throughput_window: Duration::from_millis(
                        (adaptive_config.adaptive_window_size as u64) * 100,
                    ),
                    max_wait_time: Duration::from_millis(adaptive_config.adaptive_batch_timeout_ms),
                    min_wait_time: Duration::from_millis(100), // Minimum wait time before sending a batch, even if min_batch_size is not reached.
                    adaptive_enabled: true,
                }
            }
            None => {
                // Basic reaction without adaptive batcher.
                AdaptiveBatchConfig {
                    adaptive_enabled: false,
                    ..Default::default()
                }
            }
        };

        let base = ReactionBase::new(params);

        Self {
            base,
            config: config.clone(),
            adaptive_config: adaptive_mqtt_config,
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
            "broker_addr".to_string(),
            serde_json::Value::String(self.config.broker_addr.clone()),
        );
        props.insert(
            "port".to_string(),
            serde_json::Value::Number(self.config.port.into()),
        );
        props.insert(
            "transport_mode".to_string(),
            serde_json::Value::String(format!("{:?}", self.config.transport_mode)),
        );
        props.insert(
            "keep_alive".to_string(),
            serde_json::Value::Number(self.config.keep_alive.into()),
        );
        props.insert(
            "clean_session".to_string(),
            serde_json::Value::Bool(self.config.clean_session),
        );
        props.insert(
            "request_channel_capacity".to_string(),
            serde_json::Value::Number(self.config.request_channel_capacity.into()),
        );
        props.insert(
            "event_channel_capacity".to_string(),
            serde_json::Value::Number(self.config.event_channel_capacity.into()),
        );
        props.insert(
            "pending_throttle".to_string(),
            serde_json::Value::Number(self.config.pending_throttle.into()),
        );
        props.insert(
            "connection_timeout".to_string(),
            serde_json::Value::Number(self.config.connection_timeout.into()),
        );
        props.insert(
            "max_packet_size".to_string(),
            serde_json::Value::Number(self.config.max_packet_size.into()),
        );
        props.insert(
            "max_inflight".to_string(),
            serde_json::Value::Number(self.config.max_inflight.into()),
        );
        props.insert(
            "auth_mode".to_string(),
            serde_json::Value::String(format!("{:?}", self.config.auth_mode)),
        );

        if self.adaptive_config.adaptive_enabled {
            props.insert(
                "adaptive_batching".to_string(),
                serde_json::Value::String(format!("{:?}", self.config.adaptive)),
            );
        }

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
        let shared_base = self.base.clone_shared();
        let reaction_name = self.base.id.clone();
        let adaptive_config = self.adaptive_config.clone();
        let config = self.config.clone();

        // Spawn the main processing task
        let processing_task_handle = tokio::spawn(Processor::run_internal(
            reaction_name,
            shared_base,
            adaptive_config,
            config,
            shutdown_rx,
        ));

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
