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

use super::MqttReactionBuilder;
use crate::{config::MqttReactionConfig, processor::ResultProcessor};
use anyhow::Result;
use drasi_lib::{channels::ComponentStatus, managers::log_component_start, Reaction};
use log::{error, info};
use std::sync::Arc;

use async_trait::async_trait;
use drasi_lib::reactions::{ReactionBase, ReactionBaseParams};
use serde_json::Value;
use std::collections::HashMap;

use handlebars::{template, Handlebars};
use rumqttc::{
    tokio_rustls::client,
    v5::{mqttbytes::QoS, AsyncClient, Event, Incoming, MqttOptions},
};

pub struct MqttReaction {
    pub(crate) base: ReactionBase,
    config: MqttReactionConfig,
    event_loop_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    event_loop_shutdown_tx: Arc<tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
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
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: MqttReactionConfig,
    ) -> anyhow::Result<Self> {
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
        let mut config = config;
        if config.client_id.is_none() {
            config.client_id = Some(format!("drasi-mqtt-{id}"));
        }

        config.validate(&queries)?;

        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        let base = ReactionBase::new(params);

        Ok(Self {
            base,
            config,
            event_loop_task: Arc::new(tokio::sync::Mutex::new(None)),
            event_loop_shutdown_tx: Arc::new(tokio::sync::Mutex::new(None)),
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

        if let Some(client_id) = &self.config.client_id {
            props.insert(
                "client_id".to_string(),
                serde_json::Value::String(client_id.clone()),
            );
        }

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
                .map_or(serde_json::Value::Null, |v| {
                    serde_json::Value::Number(v.into())
                }),
        );
        props.insert(
            "keep_alive".to_string(),
            self.config.keep_alive.map_or(serde_json::Value::Null, |v| {
                serde_json::Value::Number(v.into())
            }),
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
                .map_or(serde_json::Value::Null, |v| {
                    serde_json::Value::Number(v.into())
                }),
        );
        props.insert(
            "session_expiry_interval".to_string(),
            self.config
                .session_expiry_interval
                .map_or(serde_json::Value::Null, |v| {
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

        info!("[{}] MQTT reaction started", self.base.id);

        // transition to Starting
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some(format!("[{}] Starting MQTT reaction", self.base.id)),
            )
            .await;

        // clone config and base
        let config = self.config.clone();
        let base = self.base.clone_shared();

        // create the main processor which will handle results processing
        let mut processor = match ResultProcessor::new(config, base).await {
            Ok(p) => p,
            Err(e) => {
                error!(
                    "[{}] Failed to create MQTT ResultProcessor with error: {:?}, stopping reaction",
                    self.base.id, e
            );
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!(
                            "[{}] MQTT reaction stopped due to initialization failure",
                            self.base.id
                        )),
                    )
                    .await;
                return Ok(());
            }
        };

        // start the processing loop and event loop
        let (event_loop_shutdown_tx, event_loop_shutdown_rx) = tokio::sync::oneshot::channel();
        let event_loop_handle =
            if let Ok(h) = processor.start_event_loop(event_loop_shutdown_rx).await {
                h
            } else {
                error!(
                    "[{}] Failed to start MQTT event loop, stopping reaction",
                    self.base.id
                );
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!(
                            "[{}] MQTT reaction stopped due to initialization failure",
                            self.base.id
                        )),
                    )
                    .await;
                return Ok(());
            };
        *self.event_loop_task.lock().await = Some(event_loop_handle);
        *self.event_loop_shutdown_tx.lock().await = Some(event_loop_shutdown_tx);

        let processing_shutdown_rx = self.base.create_shutdown_channel().await;
        let processing_handle = if let Ok(h) = processor
            .start_processing_loop(processing_shutdown_rx)
            .await
        {
            h
        } else {
            error!(
                "[{}] Failed to start MQTT processing loop, stopping reaction",
                self.base.id
            );
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!(
                        "[{}] MQTT reaction stopped due to initialization failure",
                        self.base.id
                    )),
                )
                .await;
            return Ok(());
        };

        self.base.set_processing_task(processing_handle);

        // transition to Running
        self.base
            .set_status(
                ComponentStatus::Running,
                Some(format!("[{}] MQTT reaction started", self.base.id)),
            )
            .await;

        info!("[{}] MQTT reaction started", self.base.id);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some(format!("[{}] MQTT reaction stopping", self.base.id)),
            )
            .await;

        info!("[{}] MQTT reaction stopping", self.base.id);

        // stop the processing loop
        self.base.stop_common().await?;

        // stop the event loop and wait for it to finish aborting
        if let Some(tx) = self.event_loop_shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.event_loop_task.lock().await.take() {
            let _ = handle.await;
        }

        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some(format!("[{}] MQTT reaction stopped", self.base.id)),
            )
            .await;

        info!("[{}] MQTT reaction stopped", self.base.id);

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_config() -> MqttReactionConfig {
        MqttReactionConfig {
            url: "mqtt://localhost:1883".to_string(),
            client_id: None,
            protocol_version: crate::config::MqttProtocolVersion::V5,
            routes: std::collections::HashMap::new(),
            default_template: None,
            identity_provider: None,
            tls: None,
            event_channel_capacity: 100,
            max_inflight: None,
            keep_alive: None,
            clean_start: None,
            conn_timeout: None,
            session_expiry_interval: None,
        }
    }

    #[test]
    fn create_internal_assigns_default_client_id_when_missing() {
        let reaction = MqttReaction::create_internal(
            "rx-1".to_string(),
            vec!["q1".to_string()],
            base_config(),
            None,
            true,
        )
        .expect("reaction should be created");

        assert_eq!(
            reaction.config.client_id.as_deref(),
            Some("drasi-mqtt-rx-1")
        );
    }

    #[test]
    fn create_internal_preserves_explicit_client_id() {
        let mut config = base_config();
        config.client_id = Some("custom-client".to_string());

        let reaction = MqttReaction::create_internal(
            "rx-2".to_string(),
            vec!["q1".to_string()],
            config,
            None,
            true,
        )
        .expect("reaction should be created");

        assert_eq!(reaction.config.client_id.as_deref(), Some("custom-client"));
    }

    #[test]
    fn reaction_exposes_expected_identity_and_queries() {
        let reaction = MqttReaction::create_internal(
            "rx-3".to_string(),
            vec!["q1".to_string(), "q2".to_string()],
            base_config(),
            None,
            true,
        )
        .expect("reaction should be created");

        assert_eq!(drasi_lib::Reaction::id(&reaction), "rx-3");
        assert_eq!(drasi_lib::Reaction::type_name(&reaction), "mqtt");
        assert_eq!(drasi_lib::Reaction::query_ids(&reaction), vec!["q1", "q2"]);
        assert!(drasi_lib::Reaction::auto_start(&reaction));
    }

    #[test]
    fn from_builder_respects_auto_start_setting() {
        let reaction = MqttReaction::from_builder(
            "rx-4".to_string(),
            vec!["q1".to_string()],
            base_config(),
            None,
            false,
        )
        .expect("reaction should be created");

        assert!(!drasi_lib::Reaction::auto_start(&reaction));
    }

    #[test]
    fn properties_reflect_optional_and_protocol_fields() {
        let mut config = base_config();
        config.protocol_version = crate::config::MqttProtocolVersion::V3_1_1;
        config.keep_alive = Some(30);
        config.clean_start = Some(false);
        config.max_inflight = Some(10);
        config.conn_timeout = Some(5_000);
        config.session_expiry_interval = Some(60);

        let reaction = MqttReaction::create_internal(
            "rx-5".to_string(),
            vec!["q1".to_string()],
            config,
            None,
            true,
        )
        .expect("reaction should be created");

        let props = drasi_lib::Reaction::properties(&reaction);
        assert_eq!(props.get("client_id"), Some(&json!("drasi-mqtt-rx-5")));
        assert_eq!(props.get("url"), Some(&json!("mqtt://localhost:1883")));
        assert_eq!(props.get("protocol_version"), Some(&json!("v3_1_1")));
        assert_eq!(props.get("routes_count"), Some(&json!(0)));
        assert_eq!(props.get("has_default_template"), Some(&json!(false)));
        assert_eq!(props.get("has_identity_provider"), Some(&json!(false)));
        assert_eq!(props.get("has_tls"), Some(&json!(false)));
        assert_eq!(props.get("event_channel_capacity"), Some(&json!(100)));
        assert_eq!(props.get("max_inflight"), Some(&json!(10)));
        assert_eq!(props.get("keep_alive"), Some(&json!(30)));
        assert_eq!(props.get("clean_start"), Some(&json!(false)));
        assert_eq!(props.get("conn_timeout"), Some(&json!(5000)));
        assert_eq!(props.get("session_expiry_interval"), Some(&json!(60)));
    }
}
