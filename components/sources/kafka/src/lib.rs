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

#![allow(unexpected_cfgs)]

//! Kafka Source Plugin for Drasi
//!
//! This plugin consumes messages from Apache Kafka topics and transforms them into
//! Drasi graph change events using Handlebars template-based mapping.
//!
//! # Configuration
//!
//! | Field | Type | Default | Description |
//! |-------|------|---------|-------------|
//! | `bootstrap_servers` | string | *required* | Kafka bootstrap servers |
//! | `topic` | string | *required* | Kafka topic to consume |
//! | `group_id` | string | *required* | Consumer group ID |
//! | `node_label` | string | *required* | Default label for nodes |
//! | `mappings` | array | None | Custom mapping configuration |
//! | `security_protocol` | string | `"PLAINTEXT"` | Security protocol |
//! | `sasl_mechanism` | string | None | SASL mechanism |
//! | `sasl_username` | string | None | SASL username |
//! | `sasl_password` | string | None | SASL password |
//! | `auto_offset_reset` | string | `"earliest"` | Initial offset behavior |
//! | `additional_properties` | object | None | Extra rdkafka config |

mod config;
mod consumer;
pub mod descriptor;
mod position;

pub use config::{AutoOffsetReset, KafkaSourceConfig};
pub use position::{
    decode_partition_offsets, decode_position, encode_partition_offsets, encode_position,
    KafkaPositionComparator,
};

use async_trait::async_trait;
use config::KafkaSourceBuilder;
use consumer::KafkaConsumerTask;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::sources::{SourceBase, SourceBaseParams};
use drasi_lib::{ComponentStatus, DispatchMode, Source, SubscriptionResponse};
use drasi_source_mapping::{SourceMapping, SourceMappingEngine};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{watch, RwLock};
use tracing::{error, info, warn};

/// Kafka source plugin for Drasi.
///
/// Consumes messages from a Kafka topic and transforms them into graph change events
/// using the shared source mapping engine.
pub struct KafkaSource {
    base: SourceBase,
    config: KafkaSourceConfig,
    shutdown_tx: Arc<RwLock<Option<watch::Sender<bool>>>>,
    subscriber_resume_positions: Arc<RwLock<HashMap<String, Vec<i64>>>>,
    has_bootstrap_provider: Arc<AtomicBool>,
    await_bootstrap_boundary: Arc<AtomicBool>,
}

impl KafkaSource {
    /// Create a new KafkaSource with the given configuration.
    pub fn new(config: KafkaSourceConfig) -> anyhow::Result<Self> {
        let params = SourceBaseParams::new(&config.id).with_dispatch_mode(DispatchMode::Channel);
        Self::with_params(config, params)
    }

    pub(crate) fn with_params(
        config: KafkaSourceConfig,
        params: SourceBaseParams,
    ) -> anyhow::Result<Self> {
        let has_bootstrap_provider = params.bootstrap_provider.is_some();
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            shutdown_tx: Arc::new(RwLock::new(None)),
            subscriber_resume_positions: Arc::new(RwLock::new(HashMap::new())),
            has_bootstrap_provider: Arc::new(AtomicBool::new(has_bootstrap_provider)),
            await_bootstrap_boundary: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Create a builder for constructing a KafkaSource.
    pub fn builder(id: impl Into<String>) -> KafkaSourceBuilder {
        KafkaSourceBuilder::new(id)
    }

    /// Build the default mapping when no explicit mappings are configured.
    fn default_mapping(&self) -> SourceMapping {
        use drasi_source_mapping::{ElementTemplate, ElementType, OperationType};

        SourceMapping {
            operation: Some(OperationType::Insert),
            operation_from: None,
            operation_map: None,
            element_type: ElementType::Node,
            template: ElementTemplate {
                id: "{{key}}".to_string(),
                labels: vec![self.config.node_label.clone()],
                properties: Some(serde_json::Value::String("{{payload}}".to_string())),
                from: None,
                to: None,
            },
            when: None,
            effective_from: None,
        }
    }
}

#[async_trait]
impl Source for KafkaSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "kafka"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "bootstrap_servers".to_string(),
            serde_json::Value::String(self.config.bootstrap_servers.clone()),
        );
        props.insert(
            "topic".to_string(),
            serde_json::Value::String(self.config.topic.clone()),
        );
        props.insert(
            "group_id".to_string(),
            serde_json::Value::String(self.config.group_id.clone()),
        );
        props.insert(
            "node_label".to_string(),
            serde_json::Value::String(self.config.node_label.clone()),
        );
        if let Some(ref mappings) = self.config.mappings {
            props.insert(
                "mappings".to_string(),
                serde_json::to_value(mappings).unwrap_or_default(),
            );
        }
        if let Some(ref protocol) = self.config.security_protocol {
            props.insert(
                "security_protocol".to_string(),
                serde_json::Value::String(protocol.clone()),
            );
        }
        if let Some(ref mechanism) = self.config.sasl_mechanism {
            props.insert(
                "sasl_mechanism".to_string(),
                serde_json::Value::String(mechanism.clone()),
            );
        }
        if let Some(ref username) = self.config.sasl_username {
            props.insert(
                "sasl_username".to_string(),
                serde_json::Value::String(username.clone()),
            );
        }
        if let Some(ref password) = self.config.sasl_password {
            props.insert(
                "sasl_password".to_string(),
                serde_json::Value::String(password.clone()),
            );
        }
        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    fn supports_replay(&self) -> bool {
        true
    }

    async fn start(&self) -> anyhow::Result<()> {
        info!("[{}] Starting Kafka source", self.base.id);

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Connecting to Kafka".to_string()),
            )
            .await;

        self.base.reset_bootstrap_boundary();
        self.await_bootstrap_boundary
            .store(false, Ordering::Release);

        // Set position comparator for multi-partition replay filtering
        self.base
            .set_position_comparator(KafkaPositionComparator)
            .await;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        // Determine effective mappings
        let mappings = match &self.config.mappings {
            Some(m) => m.clone(),
            None => vec![self.default_mapping()],
        };

        let consumer_task = KafkaConsumerTask {
            config: self.config.clone(),
            mappings,
            engine: Arc::new(SourceMappingEngine::new()),
            base: self.base.clone_shared(),
            source_id: self.base.id.clone(),
            shutdown_rx,
            resume_positions: self.subscriber_resume_positions.clone(),
            await_bootstrap_boundary: self.await_bootstrap_boundary.clone(),
        };

        let source_id = self.base.id.clone();
        let base_for_status = self.base.clone_shared();

        tokio::spawn(async move {
            match consumer_task.run().await {
                Ok(()) => {
                    info!("[{source_id}] Kafka consumer task completed");
                    base_for_status
                        .set_status(ComponentStatus::Stopped, Some("Consumer stopped".into()))
                        .await;
                }
                Err(e) => {
                    error!("[{source_id}] Kafka consumer task failed: {e}");
                    base_for_status
                        .set_status(ComponentStatus::Error, Some(format!("Consumer error: {e}")))
                        .await;
                }
            }
        });

        self.base
            .set_status(ComponentStatus::Running, Some("Connected to Kafka".into()))
            .await;

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        info!("[{}] Stopping Kafka source", self.base.id);
        if let Some(tx) = self.shutdown_tx.read().await.as_ref() {
            let _ = tx.send(true);
        }
        self.base.clear_dispatchers().await;
        self.subscriber_resume_positions.write().await.clear();
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Kafka source stopped".into()),
            )
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        if let Some(ref resume_bytes) = settings.resume_from {
            match position::decode_position(resume_bytes) {
                Some((_from_partition, offsets)) => {
                    self.subscriber_resume_positions
                        .write()
                        .await
                        .insert(settings.query_id.clone(), offsets);
                }
                None => match position::decode_partition_offsets(resume_bytes) {
                    Some(partition_offsets) => {
                        let mut offsets = vec![0; partition_offsets.len()];
                        for (partition, offset) in partition_offsets {
                            if partition >= 0 && (partition as usize) < offsets.len() {
                                offsets[partition as usize] = offset;
                            }
                        }
                        self.subscriber_resume_positions
                            .write()
                            .await
                            .insert(settings.query_id.clone(), offsets);
                    }
                    None => warn!(
                        "[{}] Invalid Kafka resume position for query '{}'",
                        self.base.id, settings.query_id
                    ),
                },
            }
        }
        let should_await_boundary = settings.resume_from.is_none()
            && settings.enable_bootstrap
            && self.has_bootstrap_provider.load(Ordering::Acquire);
        if should_await_boundary {
            self.await_bootstrap_boundary.store(true, Ordering::Release);
        }
        let response = self
            .base
            .subscribe_with_bootstrap(&settings, "kafka")
            .await?;
        if should_await_boundary && response.bootstrap_receiver.is_none() {
            self.await_bootstrap_boundary
                .store(false, Ordering::Release);
        }
        Ok(response)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
        self.has_bootstrap_provider.store(true, Ordering::Release);
    }

    async fn remove_position_handle(&self, query_id: &str) {
        self.subscriber_resume_positions
            .write()
            .await
            .remove(query_id);
        self.base.remove_position_handle(query_id).await;
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "kafka-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::KafkaSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
