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

use crate::adaptive_batcher::{AdaptiveBatchConfig, AdaptiveBatcher};
use crate::config::MqttSourceConfig;
use crate::connection::MqttConnection;
use crate::processor::MqttProcessor;
use crate::MqttSourceBuilder;
use drasi_core::evaluation::functions::async_trait;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::Source;
use drasi_lib::{identity, ComponentStatus};
use std::collections::HashMap;

use anyhow::Result;
use log::{error, info};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

use drasi_lib::channels::{ComponentType, *};
use drasi_lib::SourceRuntimeContext;
use tracing::Instrument;

/// MQTT source with configurable adaptive batching.
///
/// This source acts as a subscriber for receiving data change events.
/// It supports both receiving single-event and multiple batched events, with adaptive
/// batching for optimized throughput.
///
/// # Fields
///
/// - `base`: Common source functionality (dispatchers, status, lifecycle)
/// - `config`: MQTT-specific configuration (host, port, timeout)
/// - `adaptive_config`: Adaptive batching settings for throughput optimization
pub struct MqttSource {
    /// Base source implementation providing common functionality
    base: SourceBase,
    /// MQTT source configuration
    config: MqttSourceConfig,
    /// Adaptive batching configuration for throughput optimization
    adaptive_config: AdaptiveBatchConfig,
}

impl MqttSource {
    /// Create a builder for MqttSource with the given ID.
    ///
    /// This is the recommended way to construct an MqttSource.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the source instance
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let source = MqttSource::builder("my-source")
    ///     .with_host("0.0.0.0")
    ///     .with_port(8080)
    ///     .with_topic("my/mqtt/topic")
    ///     .with_qos(MqttQoS::ONE)
    ///     .with_adaptive_enabled(true)
    ///     .with_bootstrap_provider(my_provider)
    ///     .build().await?;
    /// ```
    pub fn builder(id: impl Into<String>) -> MqttSourceBuilder {
        MqttSourceBuilder::new(id)
    }

    /// Create a new MQTT source.
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance
    /// * `config` - MQTT source configuration
    ///
    /// # Returns
    ///
    /// A new `MqttSource` instance, or an error if construction fails.
    ///
    /// # Errors
    ///
    /// Returns an error if the base source cannot be initialized.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_mqtt::{MqttSource, MqttSourceBuilder};
    ///
    /// let config = MqttSourceBuilder::new("my-mqtt-source")
    ///     .with_host("0.0.0.0")
    ///     .with_port(9001)
    ///     .with_topic("my/mqtt/topic")
    ///     .with_qos(QualityOfService::AtLeastOnce)
    ///     .build();
    ///
    /// let source = MqttSource::new("my-mqtt-source", config).await?;
    /// ```
    pub async fn new(id: impl Into<String>, config: MqttSourceConfig) -> Result<Self> {
        Self::create_internal(id, config, None, None, None).await
    }

    /// Create a new MQTT source with custom dispatch settings.
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance
    /// * `config` - MQTT source configuration
    /// * `dispatch_mode` - Optional dispatch mode (Channel, Direct, etc.)
    /// * `dispatch_buffer_capacity` - Optional buffer capacity for channel dispatch
    ///
    /// # Returns
    ///
    /// A new `MqttSource` instance with custom dispatch settings.
    ///
    /// # Errors
    ///
    /// Returns an error if the base source cannot be initialized.
    pub async fn with_dispatch(
        id: impl Into<String>,
        config: MqttSourceConfig,
        dispatch_mode: Option<DispatchMode>,
        dispatch_buffer_capacity: Option<usize>,
    ) -> Result<Self> {
        Self::create_internal(id, config, None, dispatch_mode, dispatch_buffer_capacity).await
    }

    pub async fn from_builder(
        id: impl Into<String>,
        config: MqttSourceConfig,
        adaptive_config: Option<AdaptiveBatchConfig>,
        dispatch_mode: Option<DispatchMode>,
        dispatch_buffer_capacity: Option<usize>,
    ) -> Result<Self> {
        Self::create_internal(
            id,
            config,
            adaptive_config,
            dispatch_mode,
            dispatch_buffer_capacity,
        )
        .await
    }

    async fn create_internal(
        id: impl Into<String>,
        config: MqttSourceConfig,
        adaptive_config: Option<AdaptiveBatchConfig>,
        dispatch_mode: Option<DispatchMode>,
        dispatch_buffer_capacity: Option<usize>,
    ) -> Result<Self> {
        config.validate()?;

        let id = id.into();
        let mut params = SourceBaseParams::new(id);

        // dispatching
        if let Some(mode) = dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }

        if let Some(capacity) = dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }

        // identity provider
        let base = SourceBase::new(params)?;
        if let Some(iden_prov) = &config.identity_provider {
            base.set_identity_provider(Arc::from(iden_prov.clone_box()))
                .await;
        }

        // adaptive batching
        if let Some(adaptive_config) = adaptive_config {
            Ok(Self {
                base,
                config,
                adaptive_config,
            })
        } else {
            let mut adaptive_config_internal = AdaptiveBatchConfig::default();

            if let Some(max_batch) = config.adaptive_max_batch_size {
                adaptive_config_internal.max_batch_size = max_batch;
            }
            if let Some(min_batch) = config.adaptive_min_batch_size {
                adaptive_config_internal.min_batch_size = min_batch;
            }
            if let Some(max_wait_ms) = config.adaptive_max_wait_ms {
                adaptive_config_internal.max_wait_time = Duration::from_millis(max_wait_ms);
            }
            if let Some(min_wait_ms) = config.adaptive_min_wait_ms {
                adaptive_config_internal.min_wait_time = Duration::from_millis(min_wait_ms);
            }
            if let Some(window_secs) = config.adaptive_window_secs {
                adaptive_config_internal.throughput_window = Duration::from_secs(window_secs);
            }
            if let Some(enabled) = config.adaptive_enabled {
                adaptive_config_internal.adaptive_enabled = enabled;
            }

            Ok(Self {
                base,
                config,
                adaptive_config: adaptive_config_internal,
            })
        }
    }

    pub(crate) async fn from_parts(
        base: SourceBase,
        config: MqttSourceConfig,
        adaptive_config: AdaptiveBatchConfig,
    ) -> anyhow::Result<Self> {
        config.validate()?;

        if let Some(iden_prov) = &config.identity_provider {
            base.set_identity_provider(Arc::from(iden_prov.clone_box()))
                .await;
        }

        Ok(Self {
            base,
            config,
            adaptive_config,
        })
    }

    pub fn config(&self) -> &MqttSourceConfig {
        &self.config
    }

    pub fn adaptive_config(&self) -> &AdaptiveBatchConfig {
        &self.adaptive_config
    }
}

#[async_trait]
impl Source for MqttSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "mqtt"
    }

    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();

        props.insert(
            "host".to_string(),
            serde_json::Value::String(self.config.host.clone()),
        );
        props.insert(
            "port".to_string(),
            serde_json::Value::Number(self.config.port.into()),
        );
        props.insert(
            "event_channel_capacity".to_string(),
            serde_json::Value::Number(self.config.event_channel_capacity.into()),
        );
        if let Some(adaptive_enabled) = self.config.adaptive_enabled.as_ref() {
            props.insert(
                "adaptive_enabled".to_string(),
                serde_json::Value::Bool(*adaptive_enabled),
            );
        } else {
            props.insert(
                "adaptive_enabled".to_string(),
                serde_json::Value::Bool(false),
            );
        }

        if let Some(conn_timeout) = self.config.conn_timeout.as_ref() {
            props.insert(
                "connection_timeout_seconds".to_string(),
                serde_json::Value::Number((*conn_timeout).into()),
            );
        }

        if let Some(keep_alive) = self.config.keep_alive.as_ref() {
            props.insert(
                "keep_alive_seconds".to_string(),
                serde_json::Value::Number((*keep_alive).into()),
            );
        }

        let topics_json = self
            .config
            .topics
            .iter()
            .map(|t| serde_json::Value::String(t.topic.clone()))
            .collect();
        props.insert("topics".to_string(), serde_json::Value::Array(topics_json));

        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        info!("[{}] Starting MQTT source", self.id());

        // Set Source Status to starting
        self.base.set_status(ComponentStatus::Starting).await;
        self.base
            .send_component_event(
                ComponentStatus::Starting,
                Some("Starting MQTT Source".to_string()),
            )
            .await?;

        let source_id = self.base.id.clone();
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        // Start the subscriber
        let (error_tx, error_rx) = tokio::sync::oneshot::channel();

        let span = tracing::info_span!(
            "mqtt_source_server",
            instance_id = %instance_id,
            component_id = %source_id.clone(),
            component_type = "source"
        );

        let config = self.config.clone();
        let source_id_for_task = self.id().to_string();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        let (processor_tx, processor_rx) = mpsc::channel(self.config.event_channel_capacity);

        // create batch channel.
        let batch_channel_capacity = self.adaptive_config.recommended_channel_capacity();
        let (batch_tx, batch_rx) = mpsc::channel::<SourceChangeEvent>(batch_channel_capacity);

        // start processor task
        let mut processor = MqttProcessor::new(&config);
        processor.start_processing_loop(source_id.clone(), processor_rx, batch_tx);

        // start adaptive batcher task
        let dispatchers = self.base.dispatchers.clone();
        let adaptive_config = self.adaptive_config.clone();
        processor.start_adaptive_batcher_loop(
            source_id.clone(),
            batch_rx,
            dispatchers,
            adaptive_config,
        );

        // Set the identity provider for unified access (use inside the connection task for authenticating with MQTT broker if needed).
        let identity_provider = self.base.identity_provider().await;
        // start mqtt connection task
        let server_handle = tokio::spawn(
            async move {
                let (connection_shutdown_tx, connection_shutdown_rx) =
                    tokio::sync::oneshot::channel();

                match MqttConnection::new(source_id_for_task, &config, identity_provider).await {
                    Ok((mut connection, event_loop_wrapper)) => {
                        if let Err(error) = connection
                            .run_subscription_loop(
                                connection_shutdown_rx,
                                event_loop_wrapper,
                                &config,
                                processor_tx,
                            )
                            .await
                        {
                            let _ = error_tx.send(error.to_string());
                            return;
                        }
                    }
                    Err(error) => {
                        error!("Failed to establish MQTT connection: {error:?}");
                        let _ = error_tx.send(error.to_string());
                        return;
                    }
                }

                let _ = shutdown_rx.await;
                let _ = connection_shutdown_tx.send(());
            }
            .instrument(span),
        );

        *self.base.task_handle.write().await = Some(server_handle);
        *self.base.shutdown_tx.write().await = Some(shutdown_tx);

        match timeout(Duration::from_millis(500), error_rx).await {
            Ok(Ok(error_msg)) => {
                self.base.set_status(ComponentStatus::Error).await;
                return Err(anyhow::anyhow!("{error_msg}"));
            }
            _ => {
                self.base.set_status(ComponentStatus::Running).await;
            }
        }

        // Set Source Status to running
        self.base
            .send_component_event(
                ComponentStatus::Running,
                Some(format!(
                    "MQTT source running on {}:{}",
                    self.config.host, self.config.port
                )),
            )
            .await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping MQTT source", self.base.id);

        self.base.set_status(ComponentStatus::Stopping).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopping,
                Some("Stopping MQTT source".to_string()),
            )
            .await?;

        if let Some(tx) = self.base.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }

        if let Some(handle) = self.base.task_handle.write().await.take() {
            let _ = timeout(Duration::from_secs(5), handle).await;
        }

        self.base.set_status(ComponentStatus::Stopped).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopped,
                Some("MQTT source stopped".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base.subscribe_with_bootstrap(&settings, "MQTT").await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn deprovision(&self) -> Result<()> {
        Ok(())
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }
}
