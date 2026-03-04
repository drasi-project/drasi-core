pub mod config;
pub mod connection;
pub mod model;

use config::MQTTSourceConfig;
use drasi_core::evaluation::functions::async_trait;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::queries::base;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::ComponentStatus;
use drasi_lib::Source;
use std::collections::HashMap;

use anyhow::Result;
use log::{debug, error, info, trace, warn};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::config::{
    default_auth_method, default_channel_capacity, default_host, default_port, default_qos,
    default_timeout_ms, default_topic, MQTTAuthMethod,
};
use crate::model::{MQTTElement, MqttSourceChange, QualityOfService};
use drasi_lib::channels::{ComponentType, *};
use drasi_lib::sources::common::adaptive_batcher::{AdaptiveBatchConfig, AdaptiveBatcher};
use drasi_lib::SourceRuntimeContext;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, Packet, QoS};
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
pub struct MQTTSource {
    /// Base source implementation providing common functionality
    base: SourceBase,
    /// MQTT source configuration
    config: MQTTSourceConfig,
    /// Adaptive batching configuration for throughput optimization
    adaptive_config: AdaptiveBatchConfig,
}

/// Batch event request that can accept multiple events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchEventRequest {
    pub events: Vec<MqttSourceChange>,
}

/// MQTT source app state with batching channel.
///
/// Shared state passed to the MQTTConnectionWrapper::process_events.
#[derive(Clone)]
pub struct MqttAppState {
    /// The source ID for validation against incoming requests
    source_id: String,
    /// Channel for sending events to the adaptive batcher
    batch_tx: mpsc::Sender<SourceChangeEvent>,
}

impl MQTTSource {
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
    /// A new `MQTTSource` instance, or an error if construction fails.
    ///
    /// # Errors
    ///
    /// Returns an error if the base source cannot be initialized.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_mqtt::{MQTTSource, MQTTSourceBuilder};
    ///
    /// let config = MQTTSourceBuilder::new()
    ///     .with_host("0.0.0.0")
    ///     .with_port(9001)
    ///     .with_topic("my/mqtt/topic")
    ///     .with_qos(QualityOfService::AtLeastOnce)
    ///     .build();
    ///
    /// let source = MQTTSource::new("my-mqtt-source", config)?;
    /// ```
    pub fn new(id: impl Into<String>, config: MQTTSourceConfig) -> Result<Self> {
        let id = id.into();
        let params = SourceBaseParams::new(id);

        let mut adaptive_config = AdaptiveBatchConfig::default();

        // Allow overriding adaptive parameters from config
        if let Some(max_batch) = config.adaptive_max_batch_size {
            adaptive_config.max_batch_size = max_batch;
        }
        if let Some(min_batch) = config.adaptive_min_batch_size {
            adaptive_config.min_batch_size = min_batch;
        }
        if let Some(max_wait_ms) = config.adaptive_max_wait_ms {
            adaptive_config.max_wait_time = Duration::from_millis(max_wait_ms);
        }
        if let Some(min_wait_ms) = config.adaptive_min_wait_ms {
            adaptive_config.min_wait_time = Duration::from_millis(min_wait_ms);
        }
        if let Some(window_secs) = config.adaptive_window_secs {
            adaptive_config.throughput_window = Duration::from_secs(window_secs);
        }
        if let Some(enabled) = config.adaptive_enabled {
            adaptive_config.adaptive_enabled = enabled;
        }
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            adaptive_config,
        })
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
    /// A new `MQTTSource` instance with custom dispatch settings.
    ///
    /// # Errors
    ///
    /// Returns an error if the base source cannot be initialized.
    pub fn with_dispatch(
        id: impl Into<String>,
        config: MQTTSourceConfig,
        dispatch_mode: Option<DispatchMode>,
        dispatch_buffer_capacity: Option<usize>,
    ) -> Result<Self> {
        let id = id.into();
        let mut params = SourceBaseParams::new(id);

        if let Some(mode) = dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }

        let mut adaptive_config = AdaptiveBatchConfig::default();

        if let Some(max_batch) = config.adaptive_max_batch_size {
            adaptive_config.max_batch_size = max_batch;
        }
        if let Some(min_batch) = config.adaptive_min_batch_size {
            adaptive_config.min_batch_size = min_batch;
        }
        if let Some(max_wait_ms) = config.adaptive_max_wait_ms {
            adaptive_config.max_wait_time = Duration::from_millis(max_wait_ms);
        }
        if let Some(min_wait_ms) = config.adaptive_min_wait_ms {
            adaptive_config.min_wait_time = Duration::from_millis(min_wait_ms);
        }
        if let Some(window_secs) = config.adaptive_window_secs {
            adaptive_config.throughput_window = Duration::from_secs(window_secs);
        }
        if let Some(enabled) = config.adaptive_enabled {
            adaptive_config.adaptive_enabled = enabled;
        }

        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            adaptive_config,
        })
    }

    async fn run_adaptive_batcher(
        batch_rx: mpsc::Receiver<SourceChangeEvent>,
        dispatchers: Arc<
            tokio::sync::RwLock<
                Vec<
                    Box<
                        dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync,
                    >,
                >,
            >,
        >,
        adaptive_config: AdaptiveBatchConfig,
        source_id: String,
    ) {
        let mut batcher = AdaptiveBatcher::new(batch_rx, adaptive_config.clone());
        println!("MAx wait time: {:?}", adaptive_config.max_wait_time);
        let mut total_events = 0u64;
        let mut total_batches = 0u64;

        info!(
            "[{}] MQTT Batcher with config: {:?}",
            source_id, adaptive_config
        );

        while let Some(batch) = batcher.next_batch().await {
            if batch.is_empty() {
                debug!(
                    "[{}] MQTT Batcher received empty batch, skipping",
                    source_id
                );
                continue;
            }

            let batch_size = batch.len();

            total_events += batch_size as u64;
            total_batches += 1;

            debug!(
                "[{source_id}] MQTT Batcher forwarding batch #{total_batches} with {batch_size} events to dispatchers"
            );

            let mut sent_count = 0;
            let mut failed_count = 0;

            for (idx, event) in batch.into_iter().enumerate() {
                debug!(
                    "[{}] Batch #{}, dispatching event {}/{}",
                    source_id,
                    total_batches,
                    idx + 1,
                    batch_size
                );

                let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                let wrapper = SourceEventWrapper::with_profiling(
                    event.source_id.clone(),
                    SourceEvent::Change(event.change),
                    event.timestamp,
                    profiling,
                );

                if let Err(e) =
                    SourceBase::dispatch_from_task(dispatchers.clone(), wrapper.clone(), &source_id)
                        .await
                {
                    error!(
                        "[{}] Batch #{}, failed to dispatch event {}/{} (no subscribers): {}",
                        source_id,
                        total_batches,
                        idx + 1,
                        batch_size,
                        e
                    );
                    failed_count += 1;
                } else {
                    debug!(
                        "[{}] Batch #{}, successfully dispatched event {}/{}",
                        source_id,
                        total_batches,
                        idx + 1,
                        batch_size
                    );
                    sent_count += 1;
                }
            }

            debug!(
                "[{source_id}] Batch #{total_batches} complete: {sent_count} dispatched, {failed_count} failed"
            );

            if total_batches.is_multiple_of(100) {
                info!(
                    "[{}] Adaptive MQTT metrics - Batches: {}, Events: {}, Avg batch size: {:.1}",
                    source_id,
                    total_batches,
                    total_events,
                    total_events as f64 / total_batches as f64
                );
            }
        }

        info!(
            "[{source_id}] Adaptive MQTT batcher stopped - Total batches: {total_batches}, Total events: {total_events}"
        );
    }
}

#[async_trait]
impl Source for MQTTSource {
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
            "topic".to_string(),
            serde_json::Value::String(self.config.topic.clone()),
        );
        props.insert(
            "qos".to_string(),
            serde_json::Value::String(format!("{:?}", self.config.qos)),
        );
        props.insert(
            "auth_method".to_string(),
            serde_json::Value::String(format!("{:?}", self.config.auth_method)),
        );

        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        info!("[{}] Starting MQTT source", self.id());

        self.base.set_status(ComponentStatus::Starting).await;
        self.base
            .send_component_event(
                ComponentStatus::Starting,
                Some("Starting MQTT Source".to_string()),
            )
            .await?;

        // Start the batcher
        let batch_channel_capacity = self.adaptive_config.recommended_channel_capacity();
        let (batch_tx, batch_rx) = mpsc::channel(batch_channel_capacity);
        info!(
            "[{}] MQTTSource using batch channel capacity: {} (max_batch_size: {} x 5)",
            self.base.id, batch_channel_capacity, self.adaptive_config.max_batch_size
        );

        let adaptive_config = self.adaptive_config.clone();
        let source_id = self.base.id.clone();
        let dispatchers = self.base.dispatchers.clone();

        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        info!("[{source_id}] Starting adaptive batcher task");
        let source_id_for_span = source_id.clone();
        let span = tracing::info_span!(
            "mqtt_adaptive_batcher",
            instance_id = %instance_id,
            component_id = %source_id_for_span,
            component_type = "source"
        );

        tokio::spawn(
            async move {
                Self::run_adaptive_batcher(
                    batch_rx,
                    dispatchers,
                    adaptive_config,
                    source_id_for_span,
                )
                .await
            }
            .instrument(span),
        );

        // Create MQTT State
        let state = MqttAppState {
            source_id: self.base.id.clone(),
            batch_tx,
        };

        // Start the subscriber
        let (error_tx, error_rx) = tokio::sync::oneshot::channel();
        let source_id_for_span = source_id.clone();
        let span = tracing::info_span!(
            "mqtt_source_server",
            instance_id = %instance_id,
            component_id = %source_id_for_span,
            component_type = "source"
        );
        let mqtt_conn_config = self.config.to_mqtt_connection_config(self.id().to_string());

        let server_handle = tokio::spawn(
            async move {
                let mut mqtt_connection = connection::MQTTConnectionWrapper::new(mqtt_conn_config, state);
                mqtt_connection.start(error_tx).await;
            }
            .instrument(span),
        );

        // Setup shutdown channel
        let host = self.config.host.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let host_clone = host.clone();

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

        self.base
            .send_component_event(
                ComponentStatus::Running,
                Some(format!(
                    "MQTT source running with batch support"
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

/// TODO: Complete the mqtt builder

/// Builder for MQTTSource instances.
///
/// Provides a fluent API for constructing MQTT sources with sensible defaults
/// and adaptive batching settings. The builder takes the source ID at construction
/// and returns a fully constructed `MQTTSource` from `build()`.
///
/// # Example
///
/// ```rust,ignore
/// use drasi_source_mqtt::MQTTSource;
///
/// let source = MQTTSource::builder("my-source")
///     .with_host("0.0.0.0")
///     .with_port(8080)
///     .with_topic("my/mqtt/topic")
///     .with_qos(QualityOfService::AtLeastOnce)
///     .with_adaptive_enabled(true)
///     .with_bootstrap_provider(my_provider)
///     .build()?;
/// ```
pub struct MQTTSourceBuilder {
    id: String,
    host: String,
    port: u16,
    topic: String,
    qos: QualityOfService,
    timeout_ms: u64,
    channel_capacity: usize,

    adaptive_max_batch_size: Option<usize>,
    adaptive_min_batch_size: Option<usize>,
    adaptive_max_wait_ms: Option<u64>,
    adaptive_min_wait_ms: Option<u64>,
    adaptive_window_secs: Option<u64>,
    adaptive_enabled: Option<bool>,

    auth_method: MQTTAuthMethod,
    username: Option<String>,
    password: Option<String>,

    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl MQTTSourceBuilder {
    /// Create a new MQTT source builder with the given source ID.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the source instance
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            host: default_host(),
            port: default_port(),
            topic: default_topic(),
            qos: default_qos(),
            timeout_ms: default_timeout_ms(),
            channel_capacity: default_channel_capacity(),
            adaptive_max_batch_size: None,
            adaptive_min_batch_size: None,
            adaptive_max_wait_ms: None,
            adaptive_min_wait_ms: None,
            adaptive_window_secs: None,
            adaptive_enabled: None,
            auth_method: default_auth_method(),
            username: None,
            password: None,
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    /// Set the MQTT broker host address.
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Set the MQTT broker port.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set the MQTT topic to subscribe to.
    pub fn with_topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = topic.into();
        self
    }

    /// Set the Quality of Service level for MQTT messages.
    pub fn with_qos(mut self, qos: QualityOfService) -> Self {
        self.qos = qos;
        self
    }

    /// Set the request timeout in milliseconds
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Set the internal channel buffer capacity for incoming messages
    pub fn with_channel_capacity(mut self, capacity: usize) -> Self {
        self.channel_capacity = capacity;
        self
    }

    /// Set the adaptive batching maximum batch size
    pub fn with_adaptive_max_batch_size(mut self, size: usize) -> Self {
        self.adaptive_max_batch_size = Some(size);
        self
    }

    /// Set the adaptive batching minimum batch size
    pub fn with_adaptive_min_batch_size(mut self, size: usize) -> Self {
        self.adaptive_min_batch_size = Some(size);
        self
    }

    /// Set the adaptive batching maximum wait time in milliseconds
    pub fn with_adaptive_max_wait_ms(mut self, wait_ms: u64) -> Self {
        self.adaptive_max_wait_ms = Some(wait_ms);
        self
    }

    /// Set the adaptive batching minimum wait time in milliseconds
    pub fn with_adaptive_min_wait_ms(mut self, wait_ms: u64) -> Self {
        self.adaptive_min_wait_ms = Some(wait_ms);
        self
    }

    /// Set the adaptive batching throughput window in seconds
    pub fn with_adaptive_window_secs(mut self, secs: u64) -> Self {
        self.adaptive_window_secs = Some(secs);
        self
    }

    /// Enable or disable adaptive batching
    pub fn with_adaptive_enabled(mut self, enabled: bool) -> Self {
        self.adaptive_enabled = Some(enabled);
        self
    }

    /// Set the MQTT authentication method
    pub fn with_auth_method(mut self, method: MQTTAuthMethod) -> Self {
        self.auth_method = method;
        self
    }

    /// Set the MQTT username for authentication
    pub fn with_username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Set the MQTT password for authentication
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Set the dispatch mode for event routing.
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Set the dispatch buffer capacity.
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the bootstrap provider for initial data delivery.
    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    /// Set whether this source should auto-start when DrasiLib starts.
    ///
    /// Default is `true`. Set to `false` if this source should only be
    /// started manually via `start_source()`.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: MQTTSourceConfig) -> Self {
        self.host = config.host;
        self.port = config.port;
        self.topic = config.topic;
        self.qos = config.qos;
        self.timeout_ms = config.timeout_ms;
        self.channel_capacity = config.channel_capacity;
        self.adaptive_max_batch_size = config.adaptive_max_batch_size;
        self.adaptive_min_batch_size = config.adaptive_min_batch_size;
        self.adaptive_max_wait_ms = config.adaptive_max_wait_ms;
        self.adaptive_min_wait_ms = config.adaptive_min_wait_ms;
        self.adaptive_window_secs = config.adaptive_window_secs;
        self.adaptive_enabled = config.adaptive_enabled;
        self.auth_method = config.auth_method;
        self.username = config.username;
        self.password = config.password;
        self
    }

    /// Build the MQTTSource instance.
    ///
    /// # Returns
    ///
    /// A fully constructed `MQTTSource`, or an error if construction fails.
    pub fn build(self) -> Result<MQTTSource> {
        let config = MQTTSourceConfig {
            host: self.host,
            port: self.port,
            topic: self.topic,
            qos: self.qos,
            timeout_ms: self.timeout_ms,
            channel_capacity: self.channel_capacity,
            adaptive_max_batch_size: self.adaptive_max_batch_size,
            adaptive_min_batch_size: self.adaptive_min_batch_size,
            adaptive_max_wait_ms: self.adaptive_max_wait_ms,
            adaptive_min_wait_ms: self.adaptive_min_wait_ms,
            adaptive_window_secs: self.adaptive_window_secs,
            adaptive_enabled: self.adaptive_enabled,
            auth_method: self.auth_method,
            username: self.username,
            password: self.password,
        };

        // Build SourceBaseParams with all settings
        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        // Configure adaptive batching
        let mut adaptive_config = AdaptiveBatchConfig::default();
        if let Some(max_batch) = config.adaptive_max_batch_size {
            adaptive_config.max_batch_size = max_batch;
        }
        if let Some(min_batch) = config.adaptive_min_batch_size {
            adaptive_config.min_batch_size = min_batch;
        }
        if let Some(max_wait_ms) = config.adaptive_max_wait_ms {
            adaptive_config.max_wait_time = Duration::from_millis(max_wait_ms);
        }
        if let Some(min_wait_ms) = config.adaptive_min_wait_ms {
            adaptive_config.min_wait_time = Duration::from_millis(min_wait_ms);
        }
        if let Some(window_secs) = config.adaptive_window_secs {
            adaptive_config.throughput_window = Duration::from_secs(window_secs);
        }
        if let Some(enabled) = config.adaptive_enabled {
            adaptive_config.adaptive_enabled = enabled;
        }

        // Construct the MQTTSource
        Ok(MQTTSource {
            base: SourceBase::new(params)?,
            config,
            adaptive_config,
        })
    }
}

impl MQTTSource {
    /// Create a builder for MQTTSource with the given ID.
    ///
    /// This is the recommended way to construct an MQTTSource.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the source instance
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let source = MQTTSource::builder("my-source")
    ///     .with_host("0.0.0.0")
    ///     .with_port(8080)
    ///     .with_topic("my/mqtt/topic")
    ///     .with_qos(QualityOfService::AtLeastOnce)
    ///     .with_adaptive_enabled(true)
    ///     .with_bootstrap_provider(my_provider)
    ///     .build()?;
    /// ```
    pub fn builder(id: impl Into<String>) -> MQTTSourceBuilder {
        MQTTSourceBuilder::new(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod construction {
        use super::*;

        #[test]
        fn test_builder_with_valid_config() {
            let source = MQTTSourceBuilder::new("test-source")
                .with_host("localhost")
                .with_port(8080)
                .build();
            assert!(source.is_ok());
        }

        #[test]
        fn test_builder_with_custom_config() {
            let source = MQTTSourceBuilder::new("mqtt-source")
                .with_host("0.0.0.0")
                .with_port(9000)
                .with_topic("my/mqtt/topic")
                .build()
                .unwrap();
            assert_eq!(source.id(), "mqtt-source");
        }

        #[test]
        fn test_with_dispatch_creates_source() {
            let config = MQTTSourceConfig {
                host: "localhost".to_string(),
                port: 8080,
                topic: "test/topic".to_string(),
                timeout_ms: 10000,
                qos: QualityOfService::ExactlyOnce,
                auth_method: MQTTAuthMethod::None,
                username: None,
                password: None,
                channel_capacity: 100,
                adaptive_max_batch_size: None,
                adaptive_min_batch_size: None,
                adaptive_max_wait_ms: None,
                adaptive_min_wait_ms: None,
                adaptive_window_secs: None,
                adaptive_enabled: None,
            };
            let source = MQTTSource::with_dispatch(
                "dispatch-source",
                config,
                Some(DispatchMode::Channel),
                Some(1000),
            );
            assert!(source.is_ok());
            assert_eq!(source.unwrap().id(), "dispatch-source");
        }
    }

    mod properties {
        use super::*;

        #[test]
        fn test_id_returns_correct_value() {
            let source = MQTTSourceBuilder::new("my-mqtt-source")
                .with_host("localhost")
                .build()
                .unwrap();
            assert_eq!(source.id(), "my-mqtt-source");
        }

        #[test]
        fn test_type_name_returns_mqtt() {
            let source = MQTTSourceBuilder::new("test")
                .with_host("localhost")
                .build()
                .unwrap();
            assert_eq!(source.type_name(), "mqtt");
        }

        #[test]
        fn test_properties_contains_host_and_port() {
            let source = MQTTSourceBuilder::new("test")
                .with_host("192.168.1.1")
                .with_port(9000)
                .build()
                .unwrap();
            let props = source.properties();

            assert_eq!(
                props.get("host"),
                Some(&serde_json::Value::String("192.168.1.1".to_string()))
            );
            assert_eq!(
                props.get("port"),
                Some(&serde_json::Value::Number(9000.into()))
            );
        }

        #[test]
        fn test_properties_includes_topic_when_set() {
            let source = MQTTSourceBuilder::new("test")
                .with_host("localhost")
                .with_topic("my/mqtt/topic")
                .build()
                .unwrap();
            let props = source.properties();

            assert_eq!(
                props.get("topic"),
                Some(&serde_json::Value::String("my/mqtt/topic".to_string()))
            );
        }

        #[test]
        fn test_properties_includes_topic_when_none() {
            let source = MQTTSourceBuilder::new("test")
                .with_host("localhost")
                .build()
                .unwrap();
            let props = source.properties();

            assert!(props.contains_key("topic"));
        }
    }

    mod lifecycle {
        use super::*;

        #[tokio::test]
        async fn test_initial_status_is_stopped() {
            let source = MQTTSourceBuilder::new("test")
                .with_host("localhost")
                .build()
                .unwrap();
            assert_eq!(source.status().await, ComponentStatus::Stopped);
        }
    }

    mod builder {
        use super::*;

        #[test]
        fn test_mqtt_builder_defaults() {
            let source = MQTTSourceBuilder::new("test").build().unwrap();
            assert_eq!(source.config.port, 1883);
            assert_eq!(source.config.timeout_ms, 5000);
            assert_eq!(source.config.topic, "mqtt/topic".to_string());
        }

        #[test]
        fn test_mqtt_builder_custom_values() {
            let source = MQTTSourceBuilder::new("test")
                .with_host("api.example.com")
                .with_port(9000)
                .with_topic("/webhook")
                .with_timeout_ms(5000)
                .build()
                .unwrap();

            assert_eq!(source.config.host, "api.example.com");
            assert_eq!(source.config.port, 9000);
            assert_eq!(source.config.topic, "/webhook".to_string());
            assert_eq!(source.config.timeout_ms, 5000);
        }

        #[test]
        fn test_mqtt_builder_adaptive_batching() {
            let source = MQTTSourceBuilder::new("test")
                .with_host("localhost")
                .with_adaptive_max_batch_size(1000)
                .with_adaptive_min_batch_size(10)
                .with_adaptive_max_wait_ms(500)
                .with_adaptive_min_wait_ms(50)
                .with_adaptive_window_secs(60)
                .with_adaptive_enabled(true)
                .build()
                .unwrap();

            assert_eq!(source.config.adaptive_max_batch_size, Some(1000));
            assert_eq!(source.config.adaptive_min_batch_size, Some(10));
            assert_eq!(source.config.adaptive_max_wait_ms, Some(500));
            assert_eq!(source.config.adaptive_min_wait_ms, Some(50));
            assert_eq!(source.config.adaptive_window_secs, Some(60));
            assert_eq!(source.config.adaptive_enabled, Some(true));
        }

        #[test]
        fn test_builder_id() {
            let source = MQTTSource::builder("my-mqtt-source")
                .with_host("localhost")
                .build()
                .unwrap();

            assert_eq!(source.base.id, "my-mqtt-source");
        }
    }

    mod event_conversion {
        use crate::model::convert_mqtt_to_source_change;

        use super::*;

        #[test]
        fn test_convert_node_insert() {
            let mut props = serde_json::Map::new();
            props.insert(
                "name".to_string(),
                serde_json::Value::String("Alice".to_string()),
            );
            props.insert("age".to_string(), serde_json::Value::Number(30.into()));

            let mqtt_change = MqttSourceChange::Insert {
                element: MQTTElement::Node {
                    id: "user-1".to_string(),
                    labels: vec!["User".to_string()],
                    properties: props,
                },
                timestamp: Some(1234567890000000000),
            };

            let result = convert_mqtt_to_source_change(&mqtt_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Insert { element } => match element {
                    drasi_core::models::Element::Node {
                        metadata,
                        properties,
                    } => {
                        assert_eq!(metadata.reference.element_id.as_ref(), "user-1");
                        assert_eq!(metadata.labels.len(), 1);
                        // assert_eq!(metadata.effective_from, 1234567890000); // TODO
                        assert!(properties.get("name").is_some());
                        assert!(properties.get("age").is_some());
                    }
                    _ => panic!("Expected Node element"),
                },
                _ => panic!("Expected Insert operation"),
            }
        }

        #[test]
        fn test_convert_relation_insert() {
            let mqtt_change = MqttSourceChange::Insert {
                element: MQTTElement::Relation {
                    id: "follows-1".to_string(),
                    labels: vec!["FOLLOWS".to_string()],
                    from: "user-1".to_string(),
                    to: "user-2".to_string(),
                    properties: serde_json::Map::new(),
                },
                timestamp: None,
            };

            let result = convert_mqtt_to_source_change(&mqtt_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Insert { element } => match element {
                    drasi_core::models::Element::Relation {
                        metadata,
                        out_node,
                        in_node,
                        ..
                    } => {
                        assert_eq!(metadata.reference.element_id.as_ref(), "follows-1");
                        assert_eq!(out_node.element_id.as_ref(), "user-1");
                        assert_eq!(in_node.element_id.as_ref(), "user-2");
                    }
                    _ => panic!("Expected Relation element"),
                },
                _ => panic!("Expected Insert operation"),
            }
        }

        #[test]
        fn test_convert_delete() {
            let mqtt_change = MqttSourceChange::Delete {
                id: "user-1".to_string(),
                labels: Some(vec!["User".to_string()]),
                timestamp: Some(9999999999),
            };

            let result = convert_mqtt_to_source_change(&mqtt_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Delete { metadata } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "user-1");
                    assert_eq!(metadata.labels.len(), 1);
                }
                _ => panic!("Expected Delete operation"),
            }
        }

        #[test]
        fn test_convert_update() {
            let mqtt_change = MqttSourceChange::Update {
                element: MQTTElement::Node {
                    id: "user-1".to_string(),
                    labels: vec!["User".to_string()],
                    properties: serde_json::Map::new(),
                },
                timestamp: None,
            };

            let result = convert_mqtt_to_source_change(&mqtt_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Update { .. } => {
                    // Success
                }
                _ => panic!("Expected Update operation"),
            }
        }
    }

    mod adaptive_config {
        use super::*;

        #[test]
        fn test_adaptive_config_from_mqtt_config() {
            let source = MQTTSourceBuilder::new("test")
                .with_host("localhost")
                .with_topic("test/topic")
                .with_adaptive_max_batch_size(500)
                .with_adaptive_enabled(true)
                .build()
                .unwrap();

            // The adaptive config should be initialized from the mqtt config
            assert_eq!(source.adaptive_config.max_batch_size, 500);
            assert!(source.adaptive_config.adaptive_enabled);
        }

        #[test]
        fn test_adaptive_config_uses_defaults_when_not_specified() {
            let source = MQTTSourceBuilder::new("test")
                .with_host("localhost")
                .with_topic("test/topic")
                .build()
                .unwrap();

            // Should use AdaptiveBatchConfig defaults
            let default_config = AdaptiveBatchConfig::default();
            assert_eq!(
                source.adaptive_config.max_batch_size,
                default_config.max_batch_size
            );
            assert_eq!(
                source.adaptive_config.min_batch_size,
                default_config.min_batch_size
            );
        }
    }
}
