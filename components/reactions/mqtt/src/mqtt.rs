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
use crate::config::MqttReactionConfig;
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
    event_loop_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
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
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: MqttReactionConfig) -> anyhow::Result<Self> {
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
    ) -> anyhow::Result<Self> {
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
    ) -> anyhow::Result<Self> {
        Self::create_internal(id, queries, config, priority_queue_capacity, auto_start)
    }

    /// Internal Contructor
    fn create_internal(
        id: String,
        queries: Vec<String>,
        config: MqttReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> anyhow::Result<Self> {

        config.validate(&queries)?;

        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        let base = ReactionBase::new(params);

        Ok(Self {
            base,
            config: config,
            event_loop_task: Arc::new(tokio::sync::Mutex::new(None)),
        })
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
            "url".to_string(),
            serde_json::Value::String(self.config.url.clone()),
        );
        props.insert(
            "client_id".to_string(),
            self.config
                .client_id
                .as_ref()
                .map_or(serde_json::Value::Null, |v| serde_json::Value::String(v.clone())),
        );
        props.insert(
            "protocol_version".to_string(),
            serde_json::Value::String(
                match self.config.protocol_version {
                    crate::config::MqttProtocolVersion::V5 => "v5",
                    crate::config::MqttProtocolVersion::V3_1_1 => "v3_1_1",
                }
                .to_string(),
            ),
        );
        props.insert(
            "routes_count".to_string(),
            serde_json::Value::Number((self.config.routes.len() as u64).into()),
        );
        props.insert(
            "has_default_template".to_string(),
            serde_json::Value::Bool(self.config.default_template.is_some()),
        );
        props.insert(
            "has_identity_provider".to_string(),
            serde_json::Value::Bool(self.config.identity_provider.is_some()),
        );
        props.insert(
            "has_tls".to_string(),
            serde_json::Value::Bool(self.config.tls.is_some()),
        );
        props.insert(
            "event_channel_capacity".to_string(),
            serde_json::Value::Number(self.config.event_channel_capacity.into()),
        );
        props.insert(
            "max_inflight".to_string(),
            self.config
                .max_inflight
                .map_or(serde_json::Value::Null, |v| serde_json::Value::Number(v.into())),
        );
        props.insert(
            "keep_alive".to_string(),
            self.config
                .keep_alive
                .map_or(serde_json::Value::Null, |v| serde_json::Value::Number(v.into())),
        );
        props.insert(
            "clean_start".to_string(),
            self.config
                .clean_start
                .map_or(serde_json::Value::Null, serde_json::Value::Bool),
        );
        props.insert(
            "conn_timeout".to_string(),
            self.config
                .conn_timeout
                .map_or(serde_json::Value::Null, |v| serde_json::Value::Number(v.into())),
        );
        props.insert(
            "session_expiry_interval".to_string(),
            self.config.session_expiry_interval.map_or(serde_json::Value::Null, |v| {
                serde_json::Value::Number(v.into())
            }),
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
            "[{}] MQTT reaction started",
            self.base.id
        );

        // transition to Starting
        self.base.set_status(
            ComponentStatus::Starting,
            Some(format!("[{}] Starting MQTT reaction", self.base.id)),
        ).await;

        
        // create shutdown channel for graceful termination
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // create shutdown channel for graceful termination
        let shared_base = self.base.clone_shared();
        let reaction_name = self.base.id.clone();
        let config = self.config.clone();

        // start the main processing task
        let processing_task_handle = tokio::spawn(Processor::run_internal(
            reaction_name,
            shared_base,
            config,
            shutdown_rx,
        ));

        // store the processing task handle
        self.base.set_processing_task(processing_task_handle).await;

        // transition to Running
        self.base
            .set_status(
                ComponentStatus::Running,
                Some(format!("[{}] MQTT reaction started", self.base.id)),
            )
            .await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {

        self.base.set_status(ComponentStatus::Stopping, Some(format!("[{}] MQTT reaction stopping", self.base.id))).await;


        // Use ReactionBase common stop functionality
        self.base.stop_common().await?;

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Transition to Stopped
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some(format!("[{}] MQTT reaction stopped", self.base.id)),
            )
            .await;

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
