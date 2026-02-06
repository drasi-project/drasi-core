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

use super::config::{DataType, MockSourceConfig};
use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use log::{debug, info};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

use drasi_lib::channels::*;
use drasi_lib::managers::{log_component_start, log_component_stop};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::Source;

/// Mock source that generates synthetic data for testing and development.
///
/// This source runs an internal tokio task that generates data at configurable
/// intervals. It supports different data types (counter, sensor_reading, generic) to
/// simulate various real-world scenarios.
///
/// # Fields
///
/// - `base`: Common source functionality (dispatchers, status, lifecycle)
/// - `config`: Mock-specific configuration (data_type with optional embedded sensor_count, interval_ms)
/// - `seen_sensors`: Tracks which sensors have been seen for INSERT vs UPDATE logic
pub struct MockSource {
    /// Base source implementation providing common functionality
    base: SourceBase,
    /// Mock source configuration
    config: MockSourceConfig,
    /// Tracks which sensor IDs have already been sent (for INSERT vs UPDATE logic)
    seen_sensors: Arc<RwLock<HashSet<u32>>>,
}

impl MockSource {
    /// Create a new MockSource with the given ID and configuration.
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance
    /// * `config` - Mock source configuration
    ///
    /// # Returns
    ///
    /// A new `MockSource` instance, or an error if construction fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The base source cannot be initialized
    /// - The configuration is invalid (e.g., interval_ms is 0, sensor_count is 0)
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_mock::{MockSource, MockSourceConfig, DataType};
    ///
    /// let config = MockSourceConfig {
    ///     data_type: DataType::sensor_reading(10),
    ///     interval_ms: 1000,
    /// };
    ///
    /// let source = MockSource::new("my-mock-source", config)?;
    /// ```
    pub fn new(id: impl Into<String>, config: MockSourceConfig) -> Result<Self> {
        config.validate()?;
        let id = id.into();
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            seen_sensors: Arc::new(RwLock::new(HashSet::new())),
        })
    }

    /// Create a new MockSource with custom dispatch settings
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The base source cannot be initialized
    /// - The configuration is invalid (e.g., interval_ms is 0, sensor_count is 0)
    pub fn with_dispatch(
        id: impl Into<String>,
        config: MockSourceConfig,
        dispatch_mode: Option<DispatchMode>,
        dispatch_buffer_capacity: Option<usize>,
    ) -> Result<Self> {
        config.validate()?;
        let id = id.into();
        let mut params = SourceBaseParams::new(id);
        if let Some(mode) = dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            seen_sensors: Arc::new(RwLock::new(HashSet::new())),
        })
    }
}

#[async_trait]
impl Source for MockSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "mock"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        // Convert MockSourceConfig to HashMap
        let mut props = HashMap::new();
        props.insert(
            "data_type".to_string(),
            serde_json::Value::String(self.config.data_type.to_string()),
        );
        props.insert(
            "interval_ms".to_string(),
            serde_json::Value::Number(self.config.interval_ms.into()),
        );
        if let Some(sensor_count) = self.config.data_type.sensor_count() {
            props.insert(
                "sensor_count".to_string(),
                serde_json::Value::Number(sensor_count.into()),
            );
        }
        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        log_component_start("Mock Source", &self.base.id);

        self.base.set_status(ComponentStatus::Starting).await;
        self.base
            .send_component_event(
                ComponentStatus::Starting,
                Some("Starting mock source".to_string()),
            )
            .await?;

        // Get broadcast_tx for publishing
        let base_dispatchers = self.base.dispatchers.clone();
        let source_id = self.base.id.clone();

        // Get configuration
        let data_type = self.config.data_type.clone();
        let interval_ms = self.config.interval_ms;

        // Clone seen_sensors for the task
        let seen_sensors = Arc::clone(&self.seen_sensors);

        // Start the data generation task
        let status = Arc::clone(&self.base.status);
        let source_name = self.base.id.clone();
        let task = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
            let mut seq = 0u64;

            loop {
                interval.tick().await;

                // Check if we should stop
                if !matches!(*status.read().await, ComponentStatus::Running) {
                    break;
                }

                seq += 1;

                // Generate data based on type
                let source_change = match data_type {
                    DataType::Counter => {
                        let element_id = format!("counter_{seq}");
                        let reference = ElementReference::new(&source_name, &element_id);

                        let mut property_map = ElementPropertyMap::new();
                        property_map.insert(
                            "value",
                            crate::conversion::json_to_element_value_or_default(
                                &Value::Number(seq.into()),
                                drasi_core::models::ElementValue::Null,
                            ),
                        );
                        property_map.insert(
                            "timestamp",
                            crate::conversion::json_to_element_value_or_default(
                                &Value::String(chrono::Utc::now().to_rfc3339()),
                                drasi_core::models::ElementValue::Null,
                            ),
                        );

                        let metadata = ElementMetadata {
                            reference,
                            labels: Arc::from(vec![Arc::from("Counter")]),
                            effective_from: crate::time::get_system_time_millis().unwrap_or_else(
                                |e| {
                                    log::warn!("Failed to get timestamp for mock counter: {e}");
                                    chrono::Utc::now().timestamp_millis() as u64
                                },
                            ),
                        };

                        let element = Element::Node {
                            metadata,
                            properties: property_map,
                        };

                        SourceChange::Insert { element }
                    }
                    DataType::SensorReading { sensor_count } => {
                        // Constrain sensor_id to the configured number of sensors
                        let sensor_id = rand::random::<u32>() % sensor_count;
                        // Use sensor_id as the element_id for stable identity
                        let element_id = format!("sensor_{sensor_id}");
                        let reference = ElementReference::new(&source_name, &element_id);

                        let mut property_map = ElementPropertyMap::new();
                        property_map.insert(
                            "sensor_id",
                            crate::conversion::json_to_element_value_or_default(
                                &Value::String(format!("sensor_{sensor_id}")),
                                drasi_core::models::ElementValue::Null,
                            ),
                        );
                        property_map.insert(
                            "temperature",
                            crate::conversion::json_to_element_value_or_default(
                                &Value::Number(
                                    serde_json::Number::from_f64(
                                        20.0 + rand::random::<f64>() * 10.0,
                                    )
                                    .unwrap_or(serde_json::Number::from(25)),
                                ),
                                drasi_core::models::ElementValue::Null,
                            ),
                        );
                        property_map.insert(
                            "humidity",
                            crate::conversion::json_to_element_value_or_default(
                                &Value::Number(
                                    serde_json::Number::from_f64(
                                        40.0 + rand::random::<f64>() * 20.0,
                                    )
                                    .unwrap_or(serde_json::Number::from(50)),
                                ),
                                drasi_core::models::ElementValue::Null,
                            ),
                        );
                        property_map.insert(
                            "timestamp",
                            crate::conversion::json_to_element_value_or_default(
                                &Value::String(chrono::Utc::now().to_rfc3339()),
                                drasi_core::models::ElementValue::Null,
                            ),
                        );

                        let metadata = ElementMetadata {
                            reference,
                            labels: Arc::from(vec![Arc::from("SensorReading")]),
                            effective_from: crate::time::get_system_time_millis().unwrap_or_else(
                                |e| {
                                    log::warn!("Failed to get timestamp for mock sensor: {e}");
                                    chrono::Utc::now().timestamp_millis() as u64
                                },
                            ),
                        };

                        let element = Element::Node {
                            metadata,
                            properties: property_map,
                        };

                        // Determine if this is a new sensor (Insert) or an update (Update)
                        let is_new = {
                            let mut seen = seen_sensors.write().await;
                            seen.insert(sensor_id)
                        };

                        if is_new {
                            SourceChange::Insert { element }
                        } else {
                            SourceChange::Update { element }
                        }
                    }
                    DataType::Generic => {
                        // Generic data
                        let element_id = format!("generic_{seq}");
                        let reference = ElementReference::new(&source_name, &element_id);

                        let mut property_map = ElementPropertyMap::new();
                        property_map.insert(
                            "value",
                            crate::conversion::json_to_element_value_or_default(
                                &Value::Number(rand::random::<i32>().into()),
                                drasi_core::models::ElementValue::Null,
                            ),
                        );
                        property_map.insert(
                            "message",
                            crate::conversion::json_to_element_value_or_default(
                                &Value::String("Generic mock data".to_string()),
                                drasi_core::models::ElementValue::Null,
                            ),
                        );
                        property_map.insert(
                            "timestamp",
                            crate::conversion::json_to_element_value_or_default(
                                &Value::String(chrono::Utc::now().to_rfc3339()),
                                drasi_core::models::ElementValue::Null,
                            ),
                        );

                        let metadata = ElementMetadata {
                            reference,
                            labels: Arc::from(vec![Arc::from("Generic")]),
                            effective_from: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .expect("System time is before UNIX epoch")
                                .as_nanos() as u64,
                        };

                        let element = Element::Node {
                            metadata,
                            properties: property_map,
                        };

                        SourceChange::Insert { element }
                    }
                };

                // Create profiling metadata with timestamps
                let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                let wrapper = SourceEventWrapper::with_profiling(
                    source_id.clone(),
                    SourceEvent::Change(source_change),
                    chrono::Utc::now(),
                    profiling,
                );

                // Dispatch to all subscribers via helper
                if let Err(e) =
                    SourceBase::dispatch_from_task(base_dispatchers.clone(), wrapper, &source_id)
                        .await
                {
                    debug!("Failed to dispatch change: {e}");
                }
            }

            info!("Mock source task completed");
        });

        *self.base.task_handle.write().await = Some(task);
        self.base.set_status(ComponentStatus::Running).await;

        self.base
            .send_component_event(
                ComponentStatus::Running,
                Some("Mock source started successfully".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log_component_stop("Mock Source", &self.base.id);

        self.base.set_status(ComponentStatus::Stopping).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopping,
                Some("Stopping mock source".to_string()),
            )
            .await?;

        // Cancel the task
        if let Some(handle) = self.base.task_handle.write().await.take() {
            handle.abort();
            let _ = handle.await;
        }

        self.base.set_status(ComponentStatus::Stopped).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopped,
                Some("Mock source stopped successfully".to_string()),
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
        self.base.subscribe_with_bootstrap(&settings, "Mock").await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

impl MockSource {
    /// Inject a test event into the mock source for testing purposes.
    ///
    /// This allows tests to send specific events without relying on automatic generation.
    ///
    /// # Arguments
    ///
    /// * `change` - The source change to inject
    ///
    /// # Errors
    ///
    /// Returns an error if there are no subscribers to receive the event.
    pub async fn inject_event(&self, change: SourceChange) -> Result<()> {
        self.base.dispatch_source_change(change).await
    }

    /// Create a test subscription to this source.
    ///
    /// This method delegates to SourceBase and is provided for convenience in tests.
    /// The returned receiver will receive all events generated by this source.
    ///
    /// # Returns
    ///
    /// A boxed receiver that will receive source events.
    pub fn test_subscribe(
        &self,
    ) -> Box<dyn drasi_lib::channels::ChangeReceiver<drasi_lib::channels::SourceEventWrapper>> {
        self.base.test_subscribe()
    }
}

/// Builder for MockSource instances.
///
/// Provides a fluent API for constructing mock sources with sensible defaults.
/// The builder takes the source ID at construction and returns a fully
/// constructed `MockSource` from `build()`.
///
/// # Example
///
/// ```rust,ignore
/// use drasi_source_mock::{MockSource, DataType};
///
/// let source = MockSource::builder("my-source")
///     .with_data_type(DataType::sensor_reading(10))
///     .with_interval_ms(1000)
///     .with_bootstrap_provider(my_provider)
///     .build()?;
/// ```
pub struct MockSourceBuilder {
    id: String,
    data_type: DataType,
    interval_ms: u64,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl MockSourceBuilder {
    /// Create a new builder with the given source ID.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the source instance
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            data_type: DataType::Generic,
            interval_ms: 5000,
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    /// Set the data type to generate.
    ///
    /// # Arguments
    ///
    /// * `data_type` - One of: `DataType::Counter`, `DataType::SensorReading { sensor_count }`, or `DataType::Generic` (default)
    ///
    /// For SensorReading, use `DataType::sensor_reading(count)` helper method.
    pub fn with_data_type(mut self, data_type: DataType) -> Self {
        self.data_type = data_type;
        self
    }

    /// Set the generation interval in milliseconds.
    ///
    /// # Arguments
    ///
    /// * `interval_ms` - Interval between data generation (default: 5000)
    pub fn with_interval_ms(mut self, interval_ms: u64) -> Self {
        self.interval_ms = interval_ms;
        self
    }

    /// Set the dispatch mode for event routing.
    ///
    /// # Arguments
    ///
    /// * `mode` - `Channel` (default, with backpressure) or `Broadcast`
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Set the dispatch buffer capacity.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Buffer size for dispatch channels (default: 1000)
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the bootstrap provider for initial data delivery.
    ///
    /// # Arguments
    ///
    /// * `provider` - Bootstrap provider implementation
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

    /// Build the MockSource instance.
    ///
    /// # Returns
    ///
    /// A fully constructed `MockSource`, or an error if construction fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The base source cannot be initialized
    /// - The configuration is invalid (e.g., interval_ms is 0, sensor_count is 0)
    pub fn build(self) -> Result<MockSource> {
        let config = MockSourceConfig {
            data_type: self.data_type,
            interval_ms: self.interval_ms,
        };

        config.validate()?;

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

        Ok(MockSource {
            base: SourceBase::new(params)?,
            config,
            seen_sensors: Arc::new(RwLock::new(HashSet::new())),
        })
    }
}

impl MockSource {
    /// Create a builder for MockSource with the given ID.
    ///
    /// This is the recommended way to construct a MockSource.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the source instance
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let source = MockSource::builder("my-source")
    ///     .with_data_type(DataType::sensor_reading(10))
    ///     .with_interval_ms(1000)
    ///     .build()?;
    /// ```
    pub fn builder(id: impl Into<String>) -> MockSourceBuilder {
        MockSourceBuilder::new(id)
    }
}
